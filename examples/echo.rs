use std::io;
use std::io::{Error, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::pin::pin;
use std::str::FromStr;

use async_io::{Async, IoSafe};
use async_net::TcpStream;
use futures_util::{AsyncReadExt, AsyncWriteExt, TryStreamExt};
use l3_extract::tcp::TcpAcceptor;
use l3_extract::tcp_stack::TcpStackBuilder;
use netlink_sys::SmolSocket;
use rtnetlink::Handle;
use smoltcp::wire::{Ipv4Address, Ipv4Cidr, Ipv6Cidr};
use tracing::level_filters::LevelFilter;
use tracing::{info, subscriber};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, Registry};
use tun::Layer;

const IFACE: &str = "echo0";
const IPV4: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::new(192, 168, 100, 10), 24);
const IPV4_GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 1);
const MTU: usize = 1500;

fn main() {
    async_global_executor::block_on(async {
        init_log();

        let ipv6 = Ipv6Cidr::new(Ipv6Addr::from_str("fc00::10:10").unwrap().into(), 7);
        let ipv6_gateway = Ipv6Addr::from_str("fc00::10:1").unwrap();
        let fd = create_tun(IPV4, IPV4_GATEWAY, ipv6, ipv6_gateway).await;

        info!("create tun done");

        let (mut tcp_stack, mut tcp_acceptor, _) = TcpStackBuilder::default()
            .ipv4_addr(IPV4)
            .ipv4_gateway(IPV4_GATEWAY)
            .ipv6_addr(ipv6)
            .ipv6_gateway(ipv6_gateway)
            .mtu(MTU)
            .build(fd)
            .unwrap();

        info!("create tcp stack done");

        let _stack_task = async_global_executor::spawn(async move { tcp_stack.run().await });

        ipv4_echo(&mut tcp_acceptor).await;
        ipv6_echo(&mut tcp_acceptor).await;
    });
}

async fn ipv6_echo(tcp_acceptor: &mut TcpAcceptor) {
    let connect_task = async_global_executor::spawn(async move {
        TcpStream::connect(SocketAddr::new(
            IpAddr::from_str("fc00::20:10").unwrap(),
            80,
        ))
        .await
    });

    let mut accept_tcp = tcp_acceptor.try_next().await.unwrap().unwrap();
    let mut connect_tcp = connect_task.await.unwrap();

    info!("connect and accept done");

    connect_tcp.write_all(b"test").await.unwrap();
    connect_tcp.flush().await.unwrap();
    let mut buf = [0; 4];
    accept_tcp.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf.as_slice(), b"test");

    accept_tcp.write_all(b"test").await.unwrap();
    accept_tcp.flush().await.unwrap();

    connect_tcp.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf.as_slice(), b"test");

    // equal shutdown write
    accept_tcp.close().await.unwrap();
    assert_eq!(connect_tcp.read(&mut [0; 1]).await.unwrap(), 0);
}

async fn ipv4_echo(tcp_acceptor: &mut TcpAcceptor) {
    let connect_task =
        async_global_executor::spawn(async { TcpStream::connect("192.168.200.20:80").await });

    let mut accept_tcp = tcp_acceptor.try_next().await.unwrap().unwrap();
    let mut connect_tcp = connect_task.await.unwrap();

    info!("connect and accept done");

    connect_tcp.write_all(b"test").await.unwrap();
    connect_tcp.flush().await.unwrap();
    let mut buf = [0; 4];
    accept_tcp.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf.as_slice(), b"test");

    accept_tcp.write_all(b"test").await.unwrap();
    accept_tcp.flush().await.unwrap();

    connect_tcp.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf.as_slice(), b"test");

    // equal shutdown write
    accept_tcp.close().await.unwrap();
    assert_eq!(connect_tcp.read(&mut [0; 1]).await.unwrap(), 0);
}

async fn create_tun(
    ipv4: Ipv4Cidr,
    ipv4_gateway: Ipv4Addr,
    ipv6: Ipv6Cidr,
    ipv6_gateway: Ipv6Addr,
) -> Async<SafeFd> {
    let mut tun_conf = tun::configure();
    tun_conf
        .name(IFACE)
        .layer(Layer::L3)
        .mtu(MTU as _)
        .platform(|conf| {
            conf.packet_information(false);
        });

    let tun_fd = tun::create(&tun_conf).unwrap().into_raw_fd();
    // safety: tun fd is valid
    let tun_fd = unsafe { OwnedFd::from_raw_fd(tun_fd) };
    let (conn, handle, _) = rtnetlink::new_connection_with_socket::<SmolSocket>().unwrap();
    let _netlink_task = async_global_executor::spawn(conn);
    init_tun(handle, IFACE, ipv4, ipv4_gateway, ipv6, ipv6_gateway).await;

    Async::new(SafeFd(tun_fd)).unwrap()
}

async fn init_tun(
    handle: Handle,
    iface_name: &str,
    ipv4: Ipv4Cidr,
    ipv4_gateway: Ipv4Addr,
    ipv6: Ipv6Cidr,
    ipv6_gateway: Ipv6Addr,
) {
    let link_stream = handle
        .link()
        .get()
        .match_name(iface_name.to_string())
        .execute();
    let mut link_stream = pin!(link_stream);
    let link = match link_stream.try_next().await.unwrap() {
        None => {
            panic!("tun {iface_name} created failed, it doesn't exist")
        }
        Some(link) => link,
    };
    let link_index = link.header.index;

    handle
        .address()
        .add(
            link_index,
            IpAddr::V4(Ipv4Addr::from(ipv4.address())),
            ipv4.prefix_len(),
        )
        .execute()
        .await
        .unwrap();

    handle
        .address()
        .add(
            link_index,
            IpAddr::V6(ipv6.address().into()),
            ipv6.prefix_len(),
        )
        .execute()
        .await
        .unwrap();

    handle.link().set(link_index).up().execute().await.unwrap();

    handle
        .route()
        .add()
        .v4()
        .destination_prefix(Ipv4Addr::from([0; 4]), 0)
        .gateway(ipv4_gateway)
        .execute()
        .await
        .unwrap();

    handle
        .route()
        .add()
        .v6()
        .destination_prefix(Ipv6Addr::UNSPECIFIED, 0)
        .gateway(ipv6_gateway)
        .execute()
        .await
        .unwrap();
}

fn init_log() {
    let layer = fmt::layer()
        .pretty()
        .with_target(true)
        .with_writer(io::stderr);

    let layered = Registry::default().with(layer).with(LevelFilter::DEBUG);

    subscriber::set_global_default(layered).unwrap();
}

#[derive(Debug)]
#[repr(transparent)]
struct SafeFd(OwnedFd);

unsafe impl IoSafe for SafeFd {}

impl AsFd for SafeFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for SafeFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        rustix::io::read(&self.0, buf).map_err(Error::from)
    }
}

impl Write for SafeFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        rustix::io::write(&self.0, buf).map_err(Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
