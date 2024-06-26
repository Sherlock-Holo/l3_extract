//! TCP network stack

use std::collections::HashSet;
use std::io;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use compio_buf::{IoBuf, IoBufMut};
use crossbeam_channel::SendError;
use flume::Sender;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::tcp::{RecvError, Socket as TcpSocket, SocketBuffer};
use smoltcp::socket::udp::{PacketBuffer, PacketMetadata, Socket as UdpSocket};
use smoltcp::time::Instant;
use smoltcp::wire::{
    HardwareAddress, IpCidr, IpProtocol, IpVersion, Ipv4Cidr, Ipv4Packet, Ipv6Cidr, Ipv6Packet,
    TcpPacket, UdpPacket,
};
use tracing::{debug, error, instrument};

use self::event::{EventGenerator, Events, OperationEvent};
use self::tcp::TcpAcceptor;
use self::udp::UdpAcceptor;
use crate::notify_channel::{self, NotifySender};
use crate::shared_buf::{AsIoBuf, AsIoBufMut, SharedBuf};
use crate::virtual_interface::VirtualInterface;
use crate::wake_fn::wake_once_fn;

pub mod event;
pub mod tcp;
pub mod udp;

const SOCKET_BUF_SIZE: usize = 128 * 1024;
const MTU: usize = 1500;

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub(crate) enum TypedSocketHandle {
    Tcp(SocketHandle),
    Udp(SocketHandle),
}

impl From<TypedSocketHandle> for SocketHandle {
    fn from(value: TypedSocketHandle) -> Self {
        match value {
            TypedSocketHandle::Tcp(handle) => handle,
            TypedSocketHandle::Udp(handle) => handle,
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
    remote_addr: SocketAddr,
}

/// A [`TcpStack`] builder.
#[derive(Debug, Clone, Default)]
pub struct TcpStackBuilder {
    ipv4_addr: Option<Ipv4Cidr>,
    ipv4_gateway: Option<Ipv4Addr>,

    ipv6_addr: Option<Ipv6Cidr>,
    ipv6_gateway: Option<Ipv6Addr>,

    mtu: Option<usize>,

    tcp_socket_buf_size: Option<usize>,

    checksum_capabilities: Option<ChecksumCapabilities>,
}

impl TcpStackBuilder {
    /// Set ipv4 addr.
    pub fn ipv4_addr(&mut self, ipv4_addr: Ipv4Cidr) -> &mut Self {
        self.ipv4_addr = Some(ipv4_addr);
        self
    }

    /// Set ipv4 gateway.
    pub fn ipv4_gateway(&mut self, ipv4_gateway: Ipv4Addr) -> &mut Self {
        self.ipv4_gateway = Some(ipv4_gateway);
        self
    }

    /// Set ipv6 addr.
    pub fn ipv6_addr(&mut self, ipv6_addr: Ipv6Cidr) -> &mut Self {
        self.ipv6_addr = Some(ipv6_addr);
        self
    }

    /// Set ipv6 gateway.
    pub fn ipv6_gateway(&mut self, ipv6_gateway: Ipv6Addr) -> &mut Self {
        self.ipv6_gateway = Some(ipv6_gateway);
        self
    }

    /// Set MTU, default is 1500.
    pub fn mtu(&mut self, mtu: usize) -> &mut Self {
        self.mtu = Some(mtu);
        self
    }

    /// Set TCP socket buffer size
    pub fn tcp_socket_buf_size(&mut self, socket_buf_size: usize) -> &mut Self {
        self.tcp_socket_buf_size = Some(socket_buf_size);
        self
    }

    /// Set custom checksum capabilities, default value is [`ChecksumCapabilities::default`].
    pub fn checksum_capabilities(
        &mut self,
        checksum_capabilities: ChecksumCapabilities,
    ) -> &mut Self {
        self.checksum_capabilities = Some(checksum_capabilities);
        self
    }

    /// Build a [`TcpStack`].
    pub fn build(&self) -> anyhow::Result<(TcpStack, EventGenerator, TcpAcceptor, UdpAcceptor)> {
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

        TcpStack::new(
            ipv4,
            ipv6,
            self.tcp_socket_buf_size,
            self.mtu,
            self.checksum_capabilities.clone(),
        )
    }
}

/// TCP network stack.
pub struct TcpStack {
    ipv4_addr: Option<Ipv4Addr>,
    ipv4_gateway: Option<Ipv4Addr>,

    ipv6_addr: Option<Ipv6Addr>,
    ipv6_gateway: Option<Ipv6Addr>,

    tcp_socket_buf_size: usize,

    interface: Interface,
    virtual_iface: VirtualInterface,

    socket_set: SocketSet<'static>,
    socket_handles: HashSet<TypedSocketHandle>,
    accepted_udp_sockets: HashSet<SocketAddr>,

    operation_event_tx: NotifySender<OperationEvent>,

    tcp_stream_tx: Sender<io::Result<TcpInfo>>,
    udp_stream_tx: Sender<io::Result<UdpInfo>>,
}

impl TcpStack {
    /// Create a [`TcpStackBuilder`].
    pub fn builder() -> TcpStackBuilder {
        Default::default()
    }

    /// Get virtual interface ipv4 addr.
    pub fn iface_ipv4_addr(&self) -> Option<Ipv4Addr> {
        self.ipv4_addr
    }

    /// Get virtual interface ipv4 gateway.
    pub fn iface_ipv4_gateway(&self) -> Option<Ipv4Addr> {
        self.ipv4_gateway
    }

    /// Get virtual interface ipv6 addr.
    pub fn iface_ipv6_addr(&self) -> Option<Ipv6Addr> {
        self.ipv6_addr
    }

    /// Get virtual interface ipv6 gateway.
    pub fn iface_ipv6_gateway(&self) -> Option<Ipv6Addr> {
        self.ipv6_gateway
    }

    fn new(
        ipv4: Option<(Ipv4Cidr, Ipv4Addr)>,
        ipv6: Option<(Ipv6Cidr, Ipv6Addr)>,
        tcp_socket_buf_size: Option<usize>,
        mtu: Option<usize>,
        checksum_capabilities: Option<ChecksumCapabilities>,
    ) -> anyhow::Result<(Self, EventGenerator, TcpAcceptor, UdpAcceptor)> {
        let mtu = mtu.unwrap_or(MTU);

        let mut tun_capabilities = DeviceCapabilities::default();
        tun_capabilities.max_transmission_unit = mtu;
        tun_capabilities.medium = Medium::Ip;
        if let Some(checksum_capabilities) = checksum_capabilities {
            tun_capabilities.checksum = checksum_capabilities;
        }

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

        let (operation_event_tx, operation_event_rx) = notify_channel::channel();
        let (tcp_stream_tx, tcp_stream_rx) = flume::unbounded();
        let (udp_stream_tx, udp_stream_rx) = flume::unbounded();

        let tcp_acceptor = TcpAcceptor {
            tcp_stream_rx: tcp_stream_rx.into_stream(),
            operation_event_tx: operation_event_tx.clone(),
        };
        let udp_acceptor = UdpAcceptor {
            udp_stream_rx: udp_stream_rx.into_stream(),
            operation_event_tx: operation_event_tx.clone(),
        };

        let event_generator = EventGenerator {
            receiver: operation_event_rx,
        };

        let this = Self {
            ipv4_addr: ipv4.map(|(addr, _)| addr.address().into()),
            ipv4_gateway: ipv4.map(|(_, gateway)| gateway),
            ipv6_addr: ipv6.map(|(addr, _)| addr.address().into()),
            ipv6_gateway: ipv6.map(|(_, gateway)| gateway),
            tcp_socket_buf_size: tcp_socket_buf_size.unwrap_or(SOCKET_BUF_SIZE),
            interface,
            virtual_iface,
            socket_set: SocketSet::new(vec![]),
            socket_handles: Default::default(),
            accepted_udp_sockets: Default::default(),
            operation_event_tx,
            tcp_stream_tx,
            udp_stream_tx,
        };

        Ok((this, event_generator, tcp_acceptor, udp_acceptor))
    }
}

impl TcpStack {
    /// Drive [`TcpStack`] run event loop once.
    ///
    /// `drive_once` will return packets iterator which should be sent, and the re-drive interval
    /// if [`TcpStack`] need to be driven once again
    pub fn drive_once(
        &mut self,
        packet: Option<BytesMut>,
        events: Option<Events>,
    ) -> anyhow::Result<(impl IntoIterator<Item = Bytes> + '_, Option<Duration>)> {
        if let Some(packet) = packet {
            if let Some(packet_type) = parse_packet(&packet) {
                match packet_type {
                    PacketType::Tcp { src, dst } => {
                        debug!(%src, %dst, "get new tcp syn packet");

                        self.init_tcp(dst)?;

                        debug!(%src, %dst, "add new listen socket done");
                    }

                    PacketType::Udp { src, dst } => {
                        debug!(%src, %dst, "get udp packet");

                        if self.try_init_udp(dst)? {
                            debug!(%src, %dst, "try init udp done");
                        }
                    }
                }
            }

            debug!(read_len = packet.len(), "process read io done");

            self.virtual_iface.push_receive_packet(packet);

            debug!("process packet done");
        }

        let timestamp = Instant::now();

        self.interface
            .poll(timestamp, &mut self.virtual_iface, &mut self.socket_set);

        debug!("poll interface done");

        if let Some(Events { events }) = events {
            let mut tcp_events = vec![];
            let mut udp_events = vec![];
            for event in events {
                match &event {
                    OperationEvent::Read { .. } => {
                        tcp_events.push(event);
                    }
                    OperationEvent::Write { .. } => {
                        tcp_events.push(event);
                    }

                    OperationEvent::Recv { .. } => {
                        udp_events.push(event);
                    }
                    OperationEvent::Send { .. } => {
                        udp_events.push(event);
                    }

                    OperationEvent::Ready(typed_handle) => {
                        self.handle_ready_wake_event(*typed_handle);
                    }

                    OperationEvent::Shutdown { .. } => match event {
                        OperationEvent::Shutdown { handle, result_tx } => {
                            self.handle_shutdown_event(handle, result_tx);
                        }

                        _ => unreachable!(),
                    },

                    OperationEvent::Close(typed_handle) => {
                        self.handle_close_event(*typed_handle);
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
        }

        let sleep = self.interface.poll_delay(timestamp, &self.socket_set);

        Ok((
            self.virtual_iface.pop_all_send_packets(),
            sleep.map(Into::into),
        ))
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_shutdown_event(
        &mut self,
        typed_handle: TypedSocketHandle,
        res_tx: Sender<io::Result<()>>,
    ) {
        if self.socket_handles.contains(&typed_handle) {
            match typed_handle {
                TypedSocketHandle::Tcp(handle) => {
                    let socket = self.socket_set.get_mut::<TcpSocket>(handle);
                    socket.register_send_waker(&wake_once_fn(move || {
                        let _ = res_tx.send(Ok(()));
                    }));
                    socket.close();

                    debug!("shutdown tcp socket write done");
                }

                TypedSocketHandle::Udp { .. } => {
                    unreachable!()
                }
            }

            return;
        }

        error!("unknown shutdown event");
        let _ = res_tx.send(Err(io::Error::new(
            ErrorKind::Other,
            "unknown shutdown event",
        )));
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_close_event(&mut self, typed_handle: TypedSocketHandle) {
        if self.socket_handles.contains(&typed_handle) {
            match typed_handle {
                TypedSocketHandle::Tcp(handle) => {
                    let socket = self.socket_set.get_mut::<TcpSocket>(handle);

                    socket.close();

                    debug!("shutdown tcp socket write done");

                    if !socket.is_active() {
                        self.remove_tcp_socket(handle);

                        debug!("remove tcp socket done");

                        return;
                    }

                    let tx = self.operation_event_tx.clone();
                    let waker = wake_once_fn(move || {
                        let _ = tx.send(OperationEvent::Close(TypedSocketHandle::Tcp(handle)));
                    });
                    socket.register_recv_waker(&waker);
                }

                TypedSocketHandle::Udp(handle) => {
                    let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                    let addr = socket.endpoint();
                    let addr = SocketAddr::new(
                        addr.addr.expect("udp should always bind an addr").into(),
                        addr.port,
                    );
                    socket.close();

                    self.remove_udp_socket(handle, addr);

                    debug!("close udp socket write done");
                }
            }

            return;
        }

        error!("unknown close event");
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_ready_wake_event(&mut self, typed_handle: TypedSocketHandle) {
        match typed_handle {
            TypedSocketHandle::Tcp(handle) => {
                let socket = self.socket_set.get_mut::<TcpSocket>(handle);

                if !socket.may_recv() {
                    debug!("tcp socket may send but may not recv, register recv waker and wait next waker up");

                    let tx = self.operation_event_tx.clone();
                    socket.register_recv_waker(&wake_once_fn(move || {
                        let _ = tx.send(OperationEvent::Ready(TypedSocketHandle::Tcp(handle)));
                    }));

                    return;
                }

                let local_addr = socket
                    .remote_endpoint()
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

            TypedSocketHandle::Udp(handle) => {
                let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                let addr = socket.endpoint();
                let addr = SocketAddr::new(
                    addr.addr.expect("udp should always bind an addr").into(),
                    addr.port,
                );

                let _ = self.udp_stream_tx.send(Ok(UdpInfo {
                    handle,
                    remote_addr: addr,
                }));
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_tcp_wake_events(&mut self, events: Vec<OperationEvent>) {
        for event in events {
            self.handle_tcp_wake_event(event);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_tcp_wake_event(&mut self, event: OperationEvent) {
        match event {
            OperationEvent::Write {
                handle,
                buffer,
                result_tx,
            } => {
                if !self
                    .socket_handles
                    .contains(&TypedSocketHandle::Tcp(handle))
                {
                    error!("tcp socket not found");

                    drop(buffer);
                    let _ = result_tx.send(Err(io::Error::from(ErrorKind::NotFound)));

                    return;
                }

                let socket = self.socket_set.get_mut::<TcpSocket>(handle);
                if !socket.can_send() {
                    if !socket.may_send() {
                        drop(buffer);
                        let _ = result_tx.send(Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            format!("TCP send failed, socket state: {}", socket.state()),
                        )));
                    } else {
                        let tx = self.operation_event_tx.clone();
                        socket.register_send_waker(&wake_once_fn(move || {
                            if let Err(SendError(event)) = tx.send(OperationEvent::Write {
                                handle,
                                buffer,
                                result_tx,
                            }) {
                                match event {
                                    OperationEvent::Write {
                                        buffer, result_tx, ..
                                    } => {
                                        drop(buffer);

                                        let _ = result_tx.send(Err(io::Error::new(
                                            ErrorKind::BrokenPipe,
                                            "wake TCP write failed",
                                        )));
                                    }

                                    _ => unreachable!(),
                                }
                            }
                        }));
                    }

                    return;
                }

                // Safety: when doing this operation, the other buffer is owned by future returned
                // by TcpStream::read, now the future is either blocked or dropped, no one access it
                unsafe {
                    match socket.send_slice(buffer.as_slice()) {
                        Err(err) => {
                            error!(?err, "send data failed");

                            drop(buffer);
                            let _ = result_tx.send(Err(io::Error::new(ErrorKind::Other, err)));
                        }

                        Ok(n) => {
                            drop(buffer);
                            let _ = result_tx.send(Ok(n));
                        }
                    }
                }
            }

            OperationEvent::Read {
                handle,
                buffer,
                result_tx,
            } => {
                if !self
                    .socket_handles
                    .contains(&TypedSocketHandle::Tcp(handle))
                {
                    error!("tcp socket not found");

                    drop(buffer);
                    let _ = result_tx.send(Err(io::Error::from(ErrorKind::NotFound)));

                    return;
                }

                let socket = self.socket_set.get_mut::<TcpSocket>(handle);
                if socket.recv_queue() == 0 {
                    if !socket.may_recv() {
                        drop(buffer);

                        let _ = result_tx.send(Ok(0));
                    } else {
                        let tx = self.operation_event_tx.clone();
                        socket.register_recv_waker(&wake_once_fn(move || {
                            if let Err(SendError(event)) = tx.send(OperationEvent::Read {
                                handle,
                                buffer,
                                result_tx,
                            }) {
                                match event {
                                    OperationEvent::Read {
                                        buffer, result_tx, ..
                                    } => {
                                        drop(buffer);
                                        let _ = result_tx.send(Err(io::Error::new(
                                            ErrorKind::BrokenPipe,
                                            "wake TCP read failed",
                                        )));
                                    }

                                    _ => unreachable!(),
                                }
                            }
                        }));
                    }

                    return;
                }

                // Safety: when doing this operation, the other buffer is owned by future returned
                // by TcpStream::read, now the future is either blocked or dropped, no one access it
                unsafe {
                    let res = socket.recv_slice(buffer.as_slice_mut());
                    match res {
                        Err(RecvError::Finished) => {
                            drop(buffer);

                            // when tcp read eof, still return the buffer
                            let _ = result_tx.send(Ok(0));
                        }

                        Err(err) => {
                            drop(buffer);
                            let _ = result_tx.send(Err(io::Error::new(ErrorKind::Other, err)));
                        }

                        Ok(n) => {
                            buffer.set_initiated_len(n);

                            drop(buffer);
                            let _ = result_tx.send(Ok(n));
                        }
                    }
                }
            }

            OperationEvent::Recv { .. }
            | OperationEvent::Send { .. }
            | OperationEvent::Ready(_)
            | OperationEvent::Shutdown { .. }
            | OperationEvent::Close(_) => unreachable!(),
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_udp_wake_events(&mut self, events: Vec<OperationEvent>) {
        for event in events {
            self.handle_udp_wake_event(event);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_udp_wake_event(&mut self, event: OperationEvent) {
        match event {
            OperationEvent::Send {
                handle,
                src,
                buffer,
                result_tx,
            } => {
                if !self
                    .socket_handles
                    .contains(&TypedSocketHandle::Udp(handle))
                {
                    error!("udp socket not found");

                    drop(buffer);
                    let _ = result_tx.send(Err(io::Error::from(ErrorKind::NotFound)));

                    return;
                }

                let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                if !socket.can_send() {
                    let tx = self.operation_event_tx.clone();
                    socket.register_send_waker(&wake_once_fn(move || {
                        if let Err(SendError(event)) = tx.send(OperationEvent::Send {
                            handle,
                            src,
                            buffer,
                            result_tx,
                        }) {
                            match event {
                                OperationEvent::Send {
                                    buffer, result_tx, ..
                                } => {
                                    drop(buffer);
                                    let _ = result_tx.send(Err(io::Error::new(
                                        ErrorKind::BrokenPipe,
                                        "wake UDP write failed",
                                    )));
                                }

                                _ => unreachable!(),
                            }
                        }
                    }));

                    return;
                }

                // Safety: when doing this operation, the other buffer is owned by future returned
                // by UdpSocket::recv, now the future is either blocked or dropped, no one access it
                unsafe {
                    match socket.send_slice(buffer.as_slice(), src) {
                        Err(err) => {
                            error!(?err, "send data failed");

                            drop(buffer);
                            let _ = result_tx.send(Err(io::Error::new(ErrorKind::Other, err)));
                        }

                        Ok(_) => {
                            let len = buffer.as_slice().len();
                            drop(buffer);
                            let _ = result_tx.send(Ok(len));
                        }
                    }
                }
            }

            OperationEvent::Recv {
                handle,
                buffer,
                result_tx,
            } => {
                if !self
                    .socket_handles
                    .contains(&TypedSocketHandle::Udp(handle))
                {
                    error!("udp socket not found");

                    drop(buffer);
                    let _ = result_tx.send(Err(io::Error::from(ErrorKind::NotFound)));

                    return;
                }

                let socket = self.socket_set.get_mut::<UdpSocket>(handle);
                if !socket.can_recv() {
                    let tx = self.operation_event_tx.clone();
                    socket.register_recv_waker(&wake_once_fn(move || {
                        if let Err(SendError(event)) = tx.send(OperationEvent::Recv {
                            handle,
                            buffer,
                            result_tx,
                        }) {
                            match event {
                                OperationEvent::Recv {
                                    buffer, result_tx, ..
                                } => {
                                    drop(buffer);
                                    let _ = result_tx.send(Err(io::Error::new(
                                        ErrorKind::BrokenPipe,
                                        "wake UDP read failed",
                                    )));
                                }

                                _ => unreachable!(),
                            }
                        }
                    }));

                    return;
                }

                // Safety: when doing this operation, the other buffer is owned by future returned
                // by TcpStream::read, now the future is either blocked or dropped, no one access it
                unsafe {
                    match socket.recv_slice(buffer.as_slice_mut()) {
                        Err(err) => {
                            drop(buffer);
                            let _ = result_tx.send(Err(io::Error::new(ErrorKind::Other, err)));
                        }

                        Ok((n, metadata)) => {
                            let addr = SocketAddr::new(
                                metadata.endpoint.addr.into(),
                                metadata.endpoint.port,
                            );

                            buffer.set_initiated_len(n);
                            drop(buffer);

                            let _ = result_tx.send(Ok((n, addr)));
                        }
                    }
                }
            }

            OperationEvent::Write { .. }
            | OperationEvent::Read { .. }
            | OperationEvent::Ready(_)
            | OperationEvent::Shutdown { .. }
            | OperationEvent::Close(_) => unreachable!(),
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_tcp_socket(&mut self, handle: SocketHandle) {
        self.socket_set.remove(handle);
        self.socket_handles.remove(&TypedSocketHandle::Tcp(handle));
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_udp_socket(&mut self, handle: SocketHandle, dst: SocketAddr) {
        self.socket_set.remove(handle);
        self.accepted_udp_sockets.remove(&dst);
        self.socket_handles.remove(&TypedSocketHandle::Udp(handle));
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    fn init_tcp(&mut self, dst: SocketAddr) -> anyhow::Result<()> {
        let mut socket = create_tcp_socket(self.tcp_socket_buf_size);
        socket.listen(dst)?;
        let handle = self.socket_set.add(socket);
        self.socket_handles.insert(TypedSocketHandle::Tcp(handle));
        let tx = self.operation_event_tx.clone();
        let socket = self.socket_set.get_mut::<TcpSocket>(handle);

        socket.register_send_waker(&wake_once_fn(move || {
            let _ = tx.send(OperationEvent::Ready(TypedSocketHandle::Tcp(handle)));
        }));

        Ok(())
    }

    #[instrument(level = "debug", skip(self), err(Debug))]
    fn try_init_udp(&mut self, dst: SocketAddr) -> anyhow::Result<bool> {
        if self.accepted_udp_sockets.contains(&dst) {
            debug!("ignore connected udp");

            return Ok(false);
        }

        let mut socket = create_udp_socket(SOCKET_BUF_SIZE);
        socket.bind(dst).with_context(|| "udp socket bind failed")?;
        let handle = self.socket_set.add(socket);
        let typed_handle = TypedSocketHandle::Udp(handle);
        self.socket_handles.insert(typed_handle);
        self.accepted_udp_sockets.insert(dst);

        self.operation_event_tx
            .send(OperationEvent::Ready(typed_handle))
            .map_err(|_| anyhow::anyhow!("send udp ready event failed"))?;

        Ok(true)
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

unsafe fn cast_dyn_io_buf<T: IoBuf>(buf: Arc<dyn AsIoBuf>) -> T {
    let buf = Arc::from_raw(Arc::into_raw(buf) as *const SharedBuf<T>);

    Arc::into_inner(buf)
        .expect("buf reference count not equal 1")
        .into_inner()
}

unsafe fn cast_dyn_io_buf_mut<T: IoBufMut>(buf: Arc<dyn AsIoBufMut>) -> T {
    let buf = Arc::from_raw(Arc::into_raw(buf) as *const SharedBuf<T>);

    Arc::into_inner(buf)
        .expect("buf reference count not equal 1")
        .into_inner()
}
