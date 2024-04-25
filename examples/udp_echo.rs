use std::io;
use std::io::{Error, Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::pin::pin;

use async_io::{Async, IoSafe};
use async_net::UdpSocket;
use futures_util::TryStreamExt;
use l3_extract::tcp_stack::TcpStack;
use netlink_sys::SmolSocket;
use rtnetlink::Handle;
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};
use tracing::level_filters::LevelFilter;
use tracing::{info, subscriber};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, Registry};
use tun::Layer;

const IFACE: &str = "echo0";
const IP: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::new(192, 168, 100, 10), 24);
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 1);
const MTU: usize = 1500;

fn main() {
    async_global_executor::block_on(async {
        init_log();

        let fd = create_tun().await;

        info!("create tun done");

        let (mut tcp_stack, _, mut udp_acceptor) =
            TcpStack::new(fd, IP, GATEWAY, Some(MTU)).unwrap();

        info!("create tcp stack done");

        let _stack_task = async_global_executor::spawn(async move { tcp_stack.run().await });

        let connect_task = async_global_executor::spawn(async {
            let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            udp_socket.connect("192.168.200.20:80").await?;

            info!("udp connect done");

            udp_socket.send(b"test").await.unwrap();

            info!("udp first send done");

            Ok::<_, Error>(udp_socket)
        });

        let accept_udp = udp_acceptor.try_next().await.unwrap().unwrap();
        let connect_udp = connect_task.await.unwrap();

        info!("udp connect and accept done");

        let mut buf = [0; 4];
        accept_udp.recv(&mut buf).await.unwrap();
        assert_eq!(buf.as_slice(), b"test");

        info!("connect send accept recv done");

        accept_udp.send(b"test").await.unwrap();

        info!("accept send done");

        connect_udp.recv(&mut buf).await.unwrap();
        assert_eq!(buf.as_slice(), b"test");

        info!("accept send connect recv done");
    });
}

async fn create_tun() -> Async<SafeFd> {
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
    init_tun(handle, IFACE, IP, GATEWAY).await;

    Async::new(SafeFd(tun_fd)).unwrap()
}

async fn init_tun(handle: Handle, iface_name: &str, ipv4: Ipv4Cidr, ipv4_gateway: Ipv4Addr) {
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
