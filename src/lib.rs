#![doc = include_str!("../README.md")]

pub use compio_buf::*;
#[doc(inline)]
pub use connection::Connection;
#[doc(inline)]
pub use smoltcp::phy::{Checksum, ChecksumCapabilities};
#[doc(inline)]
pub use smoltcp::wire::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
#[doc(inline)]
pub use tcp_stack::{
    tcp::{TcpAcceptor, TcpStream},
    udp::{UdpAcceptor, UdpSocket},
    TcpStack,
};
#[doc(inline)]
pub use timer::Timer;

pub mod connection;
mod notify_channel;
mod shared_buf;
pub mod tcp_stack;
pub mod timer;
mod virtual_interface;
mod wake_fn;
