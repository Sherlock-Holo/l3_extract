#![doc = include_str!("../README.md")]

#[doc(inline)]
pub use smoltcp::wire::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
#[doc(inline)]
pub use tcp_stack::{
    tcp::{TcpAcceptor, TcpStream},
    udp::{UdpAcceptor, UdpSocket},
    TcpStack,
};

mod notify_channel;
mod shared_buf;
pub mod tcp_stack;
mod virtual_interface;
mod wake_fn;
