use std::io;
use std::net::SocketAddr;

use derivative::Derivative;
use smoltcp::iface::SocketHandle;

use super::TypedSocketHandle;

pub type RecvResult = (io::Result<(usize, SocketAddr)>, Vec<u8>);

#[derive(Derivative)]
#[derivative(Debug)]
pub enum OperationEvent {
    Ready(TypedSocketHandle),

    Read {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Vec<u8>,
        result_tx: flume::Sender<(io::Result<usize>, Vec<u8>)>,
    },

    Write {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Vec<u8>,
        result_tx: flume::Sender<(io::Result<usize>, Vec<u8>)>,
    },

    Recv {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Vec<u8>,
        result_tx: flume::Sender<RecvResult>,
    },

    Send {
        handle: SocketHandle,
        src: SocketAddr,
        #[derivative(Debug = "ignore")]
        buffer: Vec<u8>,
        result_tx: flume::Sender<(io::Result<usize>, Vec<u8>)>,
    },

    Shutdown {
        handle: TypedSocketHandle,
        result_tx: flume::Sender<io::Result<()>>,
    },

    Close(TypedSocketHandle),
}
