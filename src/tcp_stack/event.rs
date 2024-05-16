use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use derivative::Derivative;
use smoltcp::iface::SocketHandle;

use super::TypedSocketHandle;
use crate::shared_buf::{AsIoBuf, AsIoBufMut};

#[derive(Derivative)]
#[derivative(Debug)]
pub enum OperationEvent {
    Ready(TypedSocketHandle),

    Read {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Arc<dyn AsIoBufMut>,
        result_tx: flume::Sender<io::Result<usize>>,
    },

    Write {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Arc<dyn AsIoBuf>,
        result_tx: flume::Sender<io::Result<usize>>,
    },

    Recv {
        handle: SocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Arc<dyn AsIoBufMut>,
        result_tx: flume::Sender<io::Result<(usize, SocketAddr)>>,
    },

    Send {
        handle: SocketHandle,
        src: SocketAddr,
        #[derivative(Debug = "ignore")]
        buffer: Arc<dyn AsIoBuf>,
        result_tx: flume::Sender<io::Result<usize>>,
    },

    Shutdown {
        handle: TypedSocketHandle,
        result_tx: flume::Sender<io::Result<()>>,
    },

    Close(TypedSocketHandle),
}
