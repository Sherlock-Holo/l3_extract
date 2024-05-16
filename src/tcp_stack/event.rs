use std::io;
use std::net::SocketAddr;

use compio_buf::{IoBuf, IoBufMut};
use derivative::Derivative;
use smoltcp::iface::SocketHandle;

use super::TypedSocketHandle;

pub type RecvResult = (
    io::Result<(usize, SocketAddr)>,
    Box<dyn IoBufMut + Send + 'static>,
);

#[derive(Derivative)]
#[derivative(Debug)]
pub enum OperationEvent {
    Ready(TypedSocketHandle),

    Read {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Box<dyn IoBufMut + Send + 'static>,
        result_tx: flume::Sender<(io::Result<usize>, Box<dyn IoBufMut + Send + 'static>)>,
    },

    Write {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Box<dyn IoBuf + Send + 'static>,
        result_tx: flume::Sender<(io::Result<usize>, Box<dyn IoBuf + Send + 'static>)>,
    },

    Recv {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Box<dyn IoBufMut + Send + 'static>,
        result_tx: flume::Sender<RecvResult>,
    },

    Send {
        handle: SocketHandle,
        src: SocketAddr,
        #[derivative(Debug = "ignore")]
        buffer: Box<dyn IoBuf + Send>,
        result_tx: flume::Sender<(io::Result<usize>, Box<dyn IoBuf + Send + 'static>)>,
    },

    Shutdown {
        handle: TypedSocketHandle,
        result_tx: flume::Sender<io::Result<()>>,
    },

    Close(TypedSocketHandle),
}
