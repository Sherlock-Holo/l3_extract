//! This crate aim to provide a way to allow users manipulate TCP and UDP which is obtained from
//! layer-3 protocol packet, like normal async TCP stream and async UDP socket.
//!
//! This crate is based on [`smoltcp`], which is a single threaded, serial and event driven
//! userspace TCP network stack crate, but this crate allows users use [`TcpStream`],
//! [`UdpSocket`] and others concurrently.
//!
//! ## Notes:
//!
//! In order to hide the [`smoltcp`] single threaded, serial and event driven implement detail, the
//! [`TcpStream`] will use channel to communicate with the [`TcpStack`], and to improve
//! performance, [`TcpStream`] has an inner buffer to reduce channel send.
//!
//! This crate put ease of use first, then performance, so it absolutely slower than kernel TCP
//! network stack, also slower than use [`smoltcp`] with hand write event loop.
//!
//! ## Examples:
//!
//! ```no_run
//! use std::net::Ipv4Addr;
//! use std::str::FromStr;
//! use futures_util::{AsyncRead, AsyncWrite, StreamExt, TryStreamExt};
//! use futures_util::stream::FuturesUnordered;
//! use futures_util::task::SpawnExt;
//! use smoltcp::wire::Ipv4Cidr;
//! use l3_extract::TcpStack;
//!
//! async fn run() {
//!     let connection = create_layer3_connection();
//!     let (mut tcp_stack, mut tcp_acceptor, mut udp_acceptor) = TcpStack::builder()
//!         .ipv4_addr(Ipv4Cidr::from_str("192.168.100.10/24").unwrap())
//!         .ipv4_gateway(Ipv4Addr::new(192, 168, 100, 1))
//!         .build(connection).unwrap();
//!     let mut futs = FuturesUnordered::new();
//!     futs.spawn(async {
//!         tcp_stack.run().await.unwrap()
//!     }).unwrap();
//!     futs.spawn(async {
//!         let tcp_stream = tcp_acceptor.try_next().await.unwrap().unwrap();
//!         // do something with tcp_stream
//!     }).unwrap();
//!     futs.spawn(async {
//!         let udp_socket = udp_acceptor.try_next().await.unwrap().unwrap();
//!         // do something with udp_socket
//!     }).unwrap();
//!
//!     futs.collect::<Vec<_>>().await;
//! }
//!
//! async fn create_layer3_connection() -> impl AsyncRead + AsyncWrite + Unpin {
//!     // create a layer3 connection
//! }
//! ```

#[doc(inline)]
pub use smoltcp::wire::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
#[doc(inline)]
pub use tcp_stack::{
    tcp::{TcpAcceptor, TcpStream},
    udp::{UdpAcceptor, UdpSocket},
    TcpStack,
};

mod notify_channel;
pub mod tcp_stack;
mod virtual_interface;
mod wake_fn;
