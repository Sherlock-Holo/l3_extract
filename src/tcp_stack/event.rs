use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use crossbeam_channel::TryRecvError;
use derivative::Derivative;
use smoltcp::iface::SocketHandle;

use super::TypedSocketHandle;
use crate::notify_channel::NotifyReceiver;
use crate::shared_buf::{AsIoBuf, AsIoBufMut};

#[derive(Debug)]
pub struct EventGenerator {
    pub(crate) receiver: NotifyReceiver<OperationEvent>,
}

#[derive(Debug, thiserror::Error)]
#[error("generate events failed: {0}")]
pub struct Error(TryRecvError);

impl EventGenerator {
    pub async fn generate(&mut self) -> Result<Events, Error> {
        self.receiver.wait().await;
        let events = self.receiver.collect_nonblock().map_err(Error)?;

        Ok(Events { events })
    }
}

#[derive(Debug)]
pub struct Events {
    pub(crate) events: Vec<OperationEvent>,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub(crate) enum OperationEvent {
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
