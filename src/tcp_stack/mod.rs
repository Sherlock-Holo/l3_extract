//! TCP network stack

use std::collections::HashSet;
use std::io;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use anyhow::Context as _;
use bytes::Buf;
use flume::Sender;
use futures_timer::Delay;
use futures_util::task::AtomicWaker;
use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, FutureExt};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{DeviceCapabilities, Medium};
use smoltcp::socket::tcp::{RecvError, Socket as TcpSocket, SocketBuffer};
use smoltcp::socket::udp::{PacketBuffer, PacketMetadata, Socket as UdpSocket};
use smoltcp::time::Instant;
use smoltcp::wire::{
    HardwareAddress, IpCidr, IpProtocol, IpVersion, Ipv4Cidr, Ipv4Packet, Ipv6Cidr, Ipv6Packet,
    TcpPacket, UdpPacket,
};
use tracing::{debug, error, instrument};

use self::tcp::TcpAcceptor;
use self::udp::UdpAcceptor;
use self::wake_event::{CloseEvent, ReadPoll, ReadyEvent, ShutdownEvent, WakeEvent, WritePoll};
use crate::notify_channel::{self, NotifyReceiver, NotifySender};
use crate::virtual_interface::VirtualInterface;
use crate::wake_fn::{wake_fn, wake_once_fn};

pub mod tcp;
pub mod tcp2;
pub mod udp;
pub mod udp2;
mod wake_event;

const SOCKET_BUF_SIZE: usize = 16 * 1024;
const MTU: usize = 1500;
const TCP_BUF_CAP: usize = 8 * 1024;

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub(crate) enum TypedSocketHandle {
    Tcp(SocketHandle),
    Udp {
        handle: SocketHandle,
        src: SocketAddr,
        dst: SocketAddr,
    },
}

impl From<TypedSocketHandle> for SocketHandle {
    fn from(value: TypedSocketHandle) -> Self {
        match value {
            TypedSocketHandle::Tcp(handle) => handle,
            TypedSocketHandle::Udp { handle, .. } => handle,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TcpInfo {
    handle: SocketHandle,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

#[derive(Debug)]
pub(crate) struct UdpInfo {
    handle: SocketHandle,
    send_payload_capacity: usize,
    recv_payload_capacity: usize,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

/// a [`TcpStack`] builder
#[derive(Debug, Clone, Default)]
pub struct TcpStackBuilder {
    ipv4_addr: Option<Ipv4Cidr>,
    ipv4_gateway: Option<Ipv4Addr>,

    ipv6_addr: Option<Ipv6Cidr>,
    ipv6_gateway: Option<Ipv6Addr>,

    mtu: Option<usize>,
    tcp_buf_cap: Option<usize>,
}

impl TcpStackBuilder {
    /// Set ipv4 addr
    pub fn ipv4_addr(&mut self, ipv4_addr: Ipv4Cidr) -> &mut Self {
        self.ipv4_addr = Some(ipv4_addr);
        self
    }

    /// Set ipv4 gateway
    pub fn ipv4_gateway(&mut self, ipv4_gateway: Ipv4Addr) -> &mut Self {
        self.ipv4_gateway = Some(ipv4_gateway);
        self
    }

    /// Set ipv6 addr
    pub fn ipv6_addr(&mut self, ipv6_addr: Ipv6Cidr) -> &mut Self {
        self.ipv6_addr = Some(ipv6_addr);
        self
    }

    /// Set ipv6 gateway
    pub fn ipv6_gateway(&mut self, ipv6_gateway: Ipv6Addr) -> &mut Self {
        self.ipv6_gateway = Some(ipv6_gateway);
        self
    }

    /// Set MTU, default is 1500
    pub fn mtu(&mut self, mtu: usize) -> &mut Self {
        self.mtu = Some(mtu);
        self
    }

    /// Set [TcpStream](tcp::TcpStream) inner buffer capacity, default is 8192
    pub fn tcp_stream_buffer_capacity(&mut self, capacity: usize) -> &mut Self {
        self.tcp_buf_cap = Some(capacity);
        self
    }

    /// Build a [`TcpStack`]
    pub fn build<C>(
        &self,
        connection: C,
    ) -> anyhow::Result<(TcpStack<C>, TcpAcceptor, UdpAcceptor)> {
        let ipv4 = match (self.ipv4_addr, self.ipv4_gateway) {
            (None, None) => None,
            (Some(ipv4_addr), Some(ipv4_gateway)) => Some((ipv4_addr, ipv4_gateway)),
            _ => {
                return Err(anyhow::anyhow!(
                    "ipv4 should set ipv4 addr and gateway together"
                ))
            }
        };

        let ipv6 = match (self.ipv6_addr, self.ipv6_gateway) {
            (None, None) => None,
            (Some(ipv6_addr), Some(ipv6_gateway)) => Some((ipv6_addr, ipv6_gateway)),
            _ => {
                return Err(anyhow::anyhow!(
                    "ipv6 should set ipv6 addr and gateway together"
                ))
            }
        };

        TcpStack::new(connection, ipv4, ipv6, self.mtu, self.tcp_buf_cap)
    }
}

/// TCP network stack
pub struct TcpStack<C> {
    ipv4_addr: Option<Ipv4Addr>,
    ipv4_gateway: Option<Ipv4Addr>,

    ipv6_addr: Option<Ipv6Addr>,
    ipv6_gateway: Option<Ipv6Addr>,

    tun_connection: C,
    tun_read_buf: Vec<u8>,

    interface: Interface,
    virtual_iface: VirtualInterface,

    socket_set: SocketSet<'static>,
    socket_handles: HashSet<TypedSocketHandle>,
    connected_udp_sockets: HashSet<(SocketAddr, SocketAddr)>,

    wake_events_tx: NotifySender<WakeEvent>,
    wake_events: NotifyReceiver<WakeEvent>,

    tcp_stream_tx: Sender<io::Result<TcpInfo>>,
    udp_stream_tx: Sender<io::Result<UdpInfo>>,
}

impl<C> TcpStack<C> {
    /// Create a [`TcpStackBuilder`]
    pub fn builder() -> TcpStackBuilder {
        Default::default()
    }

    /// Get virtual interface ipv4 addr
    pub fn iface_ipv4_addr(&self) -> Option<Ipv4Addr> {
        self.ipv4_addr
    }

    /// Get virtual interface ipv4 gateway
    pub fn iface_ipv4_gateway(&self) -> Option<Ipv4Addr> {
        self.ipv4_gateway
    }

    /// Get virtual interface ipv6 addr
    pub fn iface_ipv6_addr(&self) -> Option<Ipv6Addr> {
        self.ipv6_addr
    }

    /// Get virtual interface ipv6 gateway
    pub fn iface_ipv6_gateway(&self) -> Option<Ipv6Addr> {
        self.ipv6_gateway
    }

    fn new(
        connection: C,
        ipv4: Option<(Ipv4Cidr, Ipv4Addr)>,
        ipv6: Option<(Ipv6Cidr, Ipv6Addr)>,
        mtu: Option<usize>,
        tcp_buf_cap: Option<usize>,
    ) -> anyhow::Result<(Self, TcpAcceptor, UdpAcceptor)> {
        let mtu = mtu.unwrap_or(MTU);

        let mut tun_capabilities = DeviceCapabilities::default();
        tun_capabilities.max_transmission_unit = mtu;
        tun_capabilities.medium = Medium::Ip;

        let mut virtual_iface = VirtualInterface::new(tun_capabilities);

        let mut iface_config = Config::new(HardwareAddress::Ip);
        iface_config.random_seed = rand::random();
        let mut interface = Interface::new(iface_config, &mut virtual_iface, Instant::now());

        if let Some((ipv4_addr, ipv4_gateway)) = ipv4 {
            ipv4_init_interface(&mut interface, ipv4_addr, ipv4_gateway)?;
        }
        if let Some((ipv6_addr, ipv6_gateway)) = ipv6 {
            ipv6_init_interface(&mut interface, ipv6_addr, ipv6_gateway)?;
        }

        let (tx, rx) = notify_channel::channel();
        let (tcp_stream_tx, tcp_stream_rx) = flume::unbounded();
        let (udp_stream_tx, udp_stream_rx) = flume::unbounded();

        let tcp_acceptor = TcpAcceptor {
            tcp_stream_rx: tcp_stream_rx.into_stream(),
            wake_event_tx: tx.clone(),
            buf_cap: tcp_buf_cap.unwrap_or(TCP_BUF_CAP),
        };
        let udp_acceptor = UdpAcceptor {
            udp_stream_rx: udp_stream_rx.into_stream(),
            wake_event_tx: tx.clone(),
        };

        let this = Self {
            ipv4_addr: ipv4.map(|(addr, _)| addr.address().into()),
            ipv4_gateway: ipv4.map(|(_, gateway)| gateway),
            ipv6_addr: ipv6.map(|(addr, _)| addr.address().into()),
            ipv6_gateway: ipv6.map(|(_, gateway)| gateway),
            tun_connection: connection,
            tun_read_buf: vec![0; mtu],
            interface,
            virtual_iface,
            socket_set: SocketSet::new(vec![]),
            socket_handles: Default::default(),
            connected_udp_sockets: Default::default(),
            wake_events_tx: tx.clone(),
            wake_events: rx,
            tcp_stream_tx,
            udp_stream_tx,
        };

        Ok((this, tcp_acceptor, udp_acceptor))
    }
}

impl<C: AsyncRead + AsyncWrite + Unpin> TcpStack<C> {
    /// Drive [`TcpStack`] run event loop
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut sleep = None;
        loop {
            sleep = self.drive_one(sleep).await?;
        }
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    async fn drive_one(&mut self, sleep: Option<Duration>) -> anyhow::Result<Option<Duration>> {
        self.process_io(sleep).await?;

        debug!("process io done");

        let timestamp = Instant::now();

        self.interface
            .poll(timestamp, &mut self.virtual_iface, &mut self.socket_set);

        debug!("poll interface done");

        let events = match self.wake_events.collect_nonblock() {
            Err(err) => return Err(err).with_context(|| "broken wake socket queue"),
            Ok(events) => events,
        };

        let mut tcp_events = vec![];
        let mut udp_events = vec![];
        for event in events {
            match &event {
                WakeEvent::Read(read_event) => match read_event.handle {
                    TypedSocketHandle::Tcp(_) => {
                        tcp_events.push(event);
                    }
                    TypedSocketHandle::Udp { .. } => {
                        udp_events.push(event);
                    }
                },
                WakeEvent::Write(write_event) => match write_event.handle {
                    TypedSocketHandle::Tcp(_) => {
                        tcp_events.push(event);
                    }
                    TypedSocketHandle::Udp { .. } => {
                        udp_events.push(event);
                    }
                },

                WakeEvent::Ready(ready_event) => {
                    self.handle_ready_wake_event(ready_event);
                }

                WakeEvent::Shutdown(shutdown_event) => {
                    self.handle_shutdown_event(shutdown_event);
                }

                WakeEvent::Close(close_event) => {
                    self.handle_close_event(close_event);
                }
            }
        }

        if !tcp_events.is_empty() {
            self.handle_tcp_wake_events(tcp_events);

            debug!("handle tcp wake events done");
        }

        if !udp_events.is_empty() {
            self.handle_udp_wake_events(udp_events);

            debug!("handle udp wake events done");
        }

        self.process_write_io().await?;

        debug!("process all write io done");

        let sleep = self.interface.poll_delay(timestamp, &self.socket_set);

        Ok(sleep.map(Into::into))
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_shutdown_event(&mut self, shutdown_event: &ShutdownEvent) {
        if self.socket_handles.contains(&shutdown_event.handle) {
            match shutdown_event.handle {
                TypedSocketHandle::Tcp(handle) => {
                    let socket = self.socket_set.get_mut::<TcpSocket>(handle);
                    let _ = shutdown_event.respond.send(Poll::Ready(Ok(())));
                    socket.register_send_waker(&shutdown_event.waker);
                    socket.close();

                    debug!("shutdown tcp socket write done");
                }

                TypedSocketHandle::Udp { handle, src, dst } => {
                    let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                    let _ = shutdown_event.respond.send(Poll::Ready(Ok(())));
                    socket.register_send_waker(&shutdown_event.waker);
                    socket.close();

                    self.remove_udp_socket(handle, src, dst);

                    debug!("shutdown udp socket write done");
                }
            }

            return;
        }

        error!(?shutdown_event, "unknown shutdown event");
        let _ = shutdown_event.respond.send(Poll::Ready(Err(io::Error::new(
            ErrorKind::Other,
            "unknown shutdown event",
        ))));
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_close_event(&mut self, close_event: &CloseEvent) {
        if self.socket_handles.contains(&close_event.handle) {
            match close_event.handle {
                TypedSocketHandle::Tcp(handle) => {
                    let socket = self.socket_set.get_mut::<TcpSocket>(handle);

                    socket.close();

                    debug!("shutdown tcp socket write done");

                    if !socket.is_active() {
                        self.remove_tcp_socket(handle);

                        debug!("remove tcp socket done");

                        return;
                    }

                    let wake_events_tx = self.wake_events_tx.clone();
                    let waker = wake_once_fn(move || {
                        let _ = wake_events_tx.send(WakeEvent::Close(CloseEvent {
                            handle: TypedSocketHandle::Tcp(handle),
                        }));
                    });
                    socket.register_recv_waker(&waker);
                }

                TypedSocketHandle::Udp { handle, src, dst } => {
                    let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                    socket.close();

                    self.remove_udp_socket(handle, src, dst);

                    debug!("close udp socket write done");
                }
            }

            return;
        }

        error!(?close_event, "unknown close event");
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_ready_wake_event(&mut self, ready_event: &ReadyEvent) {
        match ready_event.handle {
            TypedSocketHandle::Tcp(handle) => {
                let socket = self.socket_set.get_mut::<TcpSocket>(handle);

                if !socket.may_recv() {
                    debug!("tcp socket may send but may not recv, register recv waker and wait next waker up");

                    let tx = self.wake_events_tx.clone();
                    socket.register_recv_waker(&wake_fn(move || {
                        let _ = tx.send(WakeEvent::Ready(ReadyEvent {
                            handle: TypedSocketHandle::Tcp(handle),
                        }));
                    }));

                    return;
                }

                let local_addr = socket
                    .local_endpoint()
                    .unwrap_or_else(|| panic!("tcp socket {handle} doesn't have local addr"));
                let local_addr = SocketAddr::new(local_addr.addr.into(), local_addr.port);
                let remote_addr = socket
                    .local_endpoint()
                    .unwrap_or_else(|| panic!("tcp socket {handle} doesn't have remote addr"));
                let remote_addr = SocketAddr::new(remote_addr.addr.into(), remote_addr.port);

                let _ = self.tcp_stream_tx.send(Ok(TcpInfo {
                    handle,
                    local_addr,
                    remote_addr,
                }));
            }

            TypedSocketHandle::Udp { handle, src, dst } => {
                let socket = self.socket_set.get::<UdpSocket>(handle);

                let _ = self.udp_stream_tx.send(Ok(UdpInfo {
                    handle,
                    send_payload_capacity: socket.payload_send_capacity(),
                    recv_payload_capacity: socket.payload_recv_capacity(),
                    local_addr: src,
                    remote_addr: dst,
                }));
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_tcp_wake_events(&mut self, events: Vec<WakeEvent>) {
        for event in events {
            self.handle_tcp_wake_event(event);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_tcp_wake_event(&mut self, event: WakeEvent) {
        match event {
            WakeEvent::Write(mut event) => {
                if !self.socket_handles.contains(&event.handle) {
                    error!(?event, "tcp socket not found");

                    let _ = event.respond.send(WritePoll::Ready {
                        buf: event.data,
                        result: Err(io::Error::from(ErrorKind::NotFound)),
                    });

                    event.waker.wake();

                    return;
                }

                let socket = self.socket_set.get_mut::<TcpSocket>(event.handle.into());
                if !socket.can_send() {
                    if !socket.may_send() {
                        let _ = event.respond.send(WritePoll::Ready {
                            buf: event.data,
                            result: Err(io::Error::new(
                                ErrorKind::BrokenPipe,
                                format!("TCP send failed, socket state: {}", socket.state()),
                            )),
                        });

                        event.waker.wake();
                    } else {
                        socket.register_send_waker(&event.waker);

                        let _ = event.respond.send(WritePoll::Pending(event.data));
                    }

                    return;
                }

                match socket.send_slice(&event.data) {
                    Err(err) => {
                        error!(?err, ?event, "send data failed");

                        let _ = event.respond.send(WritePoll::Ready {
                            buf: event.data,
                            result: Err(io::Error::new(ErrorKind::Other, err)),
                        });

                        event.waker.wake();
                    }

                    Ok(n) => {
                        event.data.advance(n);
                        let _ = event.respond.send(WritePoll::Ready {
                            buf: event.data,
                            result: Ok(()),
                        });

                        event.waker.wake();
                    }
                }
            }

            WakeEvent::Read(mut event) => {
                if !self.socket_handles.contains(&event.handle) {
                    error!(?event, "tcp socket not found");

                    let _ = event.respond.send(ReadPoll::Ready {
                        buf: event.buffer,
                        result: Err(io::Error::from(ErrorKind::NotFound)),
                    });

                    event.waker.wake();

                    return;
                }

                let socket = self.socket_set.get_mut::<TcpSocket>(event.handle.into());
                if !socket.can_recv() {
                    if !socket.may_recv() {
                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Err(io::Error::new(
                                ErrorKind::BrokenPipe,
                                format!("TCP recv failed, socket state: {}", socket.state()),
                            )),
                        });

                        event.waker.wake();
                    } else {
                        socket.register_recv_waker(&event.waker);

                        let _ = event.respond.send(ReadPoll::Pending(event.buffer));
                    }

                    return;
                }

                let res = socket.recv(|data| {
                    event.buffer.extend_from_slice(data);

                    (data.len(), ())
                });
                match res {
                    Err(RecvError::Finished) => {
                        // when tcp read eof, still return the buffer
                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Ok(()),
                        });
                    }

                    Err(err @ RecvError::InvalidState) => {
                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Err(io::Error::new(ErrorKind::Other, err)),
                        });
                    }

                    Ok(_) => {
                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Ok(()),
                        });
                    }
                }

                event.waker.wake();
            }

            WakeEvent::Ready(_) | WakeEvent::Shutdown(_) | WakeEvent::Close(_) => unreachable!(),
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_udp_wake_events(&mut self, events: Vec<WakeEvent>) {
        for event in events {
            self.handle_udp_wake_event(event);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_udp_wake_event(&mut self, event: WakeEvent) {
        match event {
            WakeEvent::Write(mut event) => {
                if !self.socket_handles.contains(&event.handle) {
                    error!(?event, "udp socket not found");

                    let _ = event.respond.send(WritePoll::Ready {
                        buf: event.data,
                        result: Err(io::Error::from(ErrorKind::NotFound)),
                    });

                    event.waker.wake();

                    return;
                }

                let socket = self.socket_set.get_mut::<UdpSocket>(event.handle.into());
                if !socket.can_send() {
                    socket.register_send_waker(&event.waker);

                    let _ = event.respond.send(WritePoll::Pending(event.data));

                    return;
                }

                let src = match event.handle {
                    TypedSocketHandle::Udp { src, .. } => src,
                    _ => unreachable!(),
                };

                match socket.send_slice(&event.data, src) {
                    Err(err) => {
                        error!(?err, ?event, "send data failed");

                        let _ = event.respond.send(WritePoll::Ready {
                            buf: event.data,
                            result: Err(io::Error::new(ErrorKind::Other, err)),
                        });
                    }

                    Ok(_) => {
                        event.data.clear();
                        let _ = event.respond.send(WritePoll::Ready {
                            buf: event.data,
                            result: Ok(()),
                        });
                    }
                }

                event.waker.wake();
            }

            WakeEvent::Read(mut event) => {
                if !self.socket_handles.contains(&event.handle) {
                    error!(?event, "udp socket not found");

                    let _ = event.respond.send(ReadPoll::Ready {
                        buf: event.buffer,
                        result: Err(io::Error::from(ErrorKind::NotFound)),
                    });

                    event.waker.wake();

                    return;
                }

                let socket = self.socket_set.get_mut::<UdpSocket>(event.handle.into());
                if !socket.can_recv() {
                    socket.register_recv_waker(&event.waker);

                    let _ = event.respond.send(ReadPoll::Pending(event.buffer));

                    return;
                }

                match socket.recv() {
                    Err(err) => {
                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Err(io::Error::new(ErrorKind::Other, err)),
                        });
                    }

                    Ok((data, _)) => {
                        event.buffer.extend_from_slice(data);

                        let _ = event.respond.send(ReadPoll::Ready {
                            buf: event.buffer,
                            result: Ok(()),
                        });
                    }
                }

                event.waker.wake();
            }

            WakeEvent::Ready(_) | WakeEvent::Shutdown(_) | WakeEvent::Close(_) => unreachable!(),
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_tcp_socket(&mut self, handle: SocketHandle) {
        self.socket_set.remove(handle);
        self.socket_handles.remove(&TypedSocketHandle::Tcp(handle));
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_udp_socket(&mut self, handle: SocketHandle, src: SocketAddr, dst: SocketAddr) {
        self.socket_set.remove(handle);
        self.connected_udp_sockets.remove(&(src, dst));
        self.socket_handles
            .remove(&TypedSocketHandle::Udp { handle, src, dst });
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    async fn process_io(&mut self, sleep: Option<Duration>) -> anyhow::Result<()> {
        if let Some(n) = self.process_read_io(sleep).await? {
            let buf = &self.tun_read_buf[..n];
            self.virtual_iface.push_receive_packet(buf);

            if let Some(packet_type) = parse_packet(buf) {
                match packet_type {
                    PacketType::Tcp { src, dst } => {
                        debug!(%src, %dst, "get new tcp syn packet");

                        self.init_tcp(dst)?;

                        debug!(%src, %dst, "add new listen socket done");
                    }

                    PacketType::Udp { src, dst } => {
                        debug!(%src, %dst, "get udp packet");

                        self.try_init_udp(src, dst)?;

                        debug!(%src, %dst, "try init udp done");
                    }
                }
            }

            debug!(n, "process read io done");
        }

        self.process_write_io().await?;

        debug!("process write io done");

        Ok(())
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    fn init_tcp(&mut self, dst: SocketAddr) -> anyhow::Result<()> {
        let mut socket = create_tcp_socket(SOCKET_BUF_SIZE);
        socket.listen(dst)?;
        let handle = self.socket_set.add(socket);
        self.socket_handles.insert(TypedSocketHandle::Tcp(handle));
        let tx = self.wake_events_tx.clone();
        let socket = self.socket_set.get_mut::<TcpSocket>(handle);

        socket.register_send_waker(&wake_fn(move || {
            let _ = tx.send(WakeEvent::Ready(ReadyEvent {
                handle: TypedSocketHandle::Tcp(handle),
            }));
        }));

        Ok(())
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    fn try_init_udp(&mut self, src: SocketAddr, dst: SocketAddr) -> anyhow::Result<()> {
        if self.connected_udp_sockets.contains(&(src, dst)) {
            debug!("ignore connected udp");

            return Ok(());
        }

        let mut socket = create_udp_socket(SOCKET_BUF_SIZE);
        socket.bind(dst).with_context(|| "udp socket bind failed")?;
        let handle = self.socket_set.add(socket);
        let typed_handle = TypedSocketHandle::Udp { handle, src, dst };
        self.socket_handles.insert(typed_handle);
        self.connected_udp_sockets.insert((src, dst));

        self.wake_events_tx.send(WakeEvent::Ready(ReadyEvent {
            handle: typed_handle,
        }))?;

        Ok(())
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    async fn process_write_io(&mut self) -> anyhow::Result<()> {
        while let Some(packet) = self.virtual_iface.peek_send_packet() {
            self.tun_connection
                .write(&packet)
                .await
                .with_context(|| "write packet to tun failed")?;

            self.virtual_iface.consume_send_packet();
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self), ret, err(Debug))]
    async fn process_read_io(&mut self, sleep: Option<Duration>) -> anyhow::Result<Option<usize>> {
        let events_wait = self.wake_events.wait();
        let read = self.tun_connection.read(&mut self.tun_read_buf);
        let n = match sleep {
            None => {
                futures_util::select! {
                    _ = events_wait.fuse() => return Ok(None),
                    res = read.fuse() => res?
                }
            }

            Some(delay) => {
                let mut timer = Delay::new(delay).fuse();
                futures_util::select! {
                    _ = timer => return Ok(None),
                    _ = events_wait.fuse() => return Ok(None),
                    res = read.fuse() => res?
                }
            }
        };
        if n == 0 {
            return Err(anyhow::anyhow!("tun is broken"));
        }

        Ok(Some(n))
    }
}

#[derive(Debug)]
enum PacketType {
    Tcp { src: SocketAddr, dst: SocketAddr },
    Udp { src: SocketAddr, dst: SocketAddr },
}

fn parse_packet(buf: &[u8]) -> Option<PacketType> {
    let ip_version = IpVersion::of_packet(buf).ok()?;
    match ip_version {
        IpVersion::Ipv4 => {
            let packet = Ipv4Packet::new_checked(buf).ok()?;
            let payload = packet.payload();
            let src_addr = Ipv4Addr::from(packet.src_addr());
            let dst_addr = Ipv4Addr::from(packet.dst_addr());

            match packet.next_header() {
                IpProtocol::Tcp => {
                    let tcp_packet = TcpPacket::new_checked(payload).ok()?;
                    if tcp_packet.syn() && !tcp_packet.ack() {
                        let src_port = tcp_packet.src_port();
                        let src = SocketAddrV4::new(src_addr, src_port);
                        let dst_port = tcp_packet.dst_port();
                        let dst = SocketAddrV4::new(dst_addr, dst_port);

                        Some(PacketType::Tcp {
                            src: src.into(),
                            dst: dst.into(),
                        })
                    } else {
                        None
                    }
                }

                IpProtocol::Udp => {
                    let udp_packet = UdpPacket::new_checked(payload).ok()?;
                    let src_port = udp_packet.src_port();
                    let src = SocketAddrV4::new(src_addr, src_port);
                    let dst_port = udp_packet.dst_port();
                    let dst = SocketAddrV4::new(dst_addr, dst_port);

                    Some(PacketType::Udp {
                        src: src.into(),
                        dst: dst.into(),
                    })
                }

                _ => None,
            }
        }

        IpVersion::Ipv6 => {
            let packet = Ipv6Packet::new_checked(buf).ok()?;
            let payload = packet.payload();
            let src_addr = Ipv6Addr::from(packet.src_addr());
            let dst_addr = Ipv6Addr::from(packet.dst_addr());

            match packet.next_header() {
                IpProtocol::Tcp => {
                    let tcp_packet = TcpPacket::new_checked(payload).ok()?;
                    if tcp_packet.syn() && !tcp_packet.ack() {
                        let src_port = tcp_packet.src_port();
                        let src = SocketAddrV6::new(src_addr, src_port, 0, 0);
                        let dst_port = tcp_packet.dst_port();
                        let dst = SocketAddrV6::new(dst_addr, dst_port, 0, 0);

                        Some(PacketType::Tcp {
                            src: src.into(),
                            dst: dst.into(),
                        })
                    } else {
                        None
                    }
                }

                IpProtocol::Udp => {
                    let udp_packet = UdpPacket::new_checked(payload).ok()?;
                    let src_port = udp_packet.src_port();
                    let src = SocketAddrV6::new(src_addr, src_port, 0, 0);
                    let dst_port = udp_packet.dst_port();
                    let dst = SocketAddrV6::new(dst_addr, dst_port, 0, 0);

                    Some(PacketType::Udp {
                        src: src.into(),
                        dst: dst.into(),
                    })
                }

                _ => None,
            }
        }
    }
}

fn create_tcp_socket(socket_buf_size: usize) -> TcpSocket<'static> {
    let tx_socket_buf = SocketBuffer::new(vec![0; socket_buf_size]);
    let rx_socket_buf = SocketBuffer::new(vec![0; socket_buf_size]);
    let mut socket = TcpSocket::new(rx_socket_buf, tx_socket_buf);
    socket.set_nagle_enabled(false);

    socket
}

fn create_udp_socket(socket_buf_size: usize) -> UdpSocket<'static> {
    let tx_socket_buf =
        PacketBuffer::new(vec![PacketMetadata::EMPTY; 4096], vec![0; socket_buf_size]);
    let rx_socket_buf =
        PacketBuffer::new(vec![PacketMetadata::EMPTY; 4096], vec![0; socket_buf_size]);

    UdpSocket::new(rx_socket_buf, tx_socket_buf)
}

#[instrument(level = "debug", skip(interface), err(Debug))]
fn ipv4_init_interface(
    interface: &mut Interface,
    ipv4: Ipv4Cidr,
    gateway: Ipv4Addr,
) -> anyhow::Result<()> {
    interface.set_any_ip(true);
    interface.update_ip_addrs(|addrs| {
        // addrs.push(IpCidr::Ipv4(ipv4)).unwrap();
        addrs
            .push(IpCidr::Ipv4(Ipv4Cidr::new(
                gateway.into(),
                ipv4.prefix_len(),
            )))
            .unwrap();
    });
    interface
        .routes_mut()
        .add_default_ipv4_route(gateway.into())?;

    Ok(())
}

#[instrument(level = "debug", skip(interface), err(Debug))]
fn ipv6_init_interface(
    interface: &mut Interface,
    ipv6: Ipv6Cidr,
    gateway: Ipv6Addr,
) -> anyhow::Result<()> {
    interface.set_any_ip(true);
    interface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::Ipv6(ipv6)).unwrap();
        /*addrs
        .push(IpCidr::Ipv6(Ipv6Cidr::new(
            gateway.into(),
            ipv6.prefix_len(),
        )))
        .unwrap();*/
    });
    interface
        .routes_mut()
        .add_default_ipv6_route(gateway.into())?;

    Ok(())
}

fn create_share_waker(atomic_waker: Arc<AtomicWaker>) -> Waker {
    wake_fn(move || {
        atomic_waker.wake();
    })
}
