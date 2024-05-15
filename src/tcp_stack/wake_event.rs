use std::io;

use derivative::Derivative;

use super::TypedSocketHandle;

#[derive(Derivative)]
#[derivative(Debug)]
pub enum OperationEvent {
    Ready(TypedSocketHandle),

    Read {
        handle: TypedSocketHandle,
        #[derivative(Debug = "ignore")]
        buffer: Vec<u8>,
        result_tx: flume::Sender<(io::Result<usize>, Vec<u8>)>,
    },

    Write {
        handle: TypedSocketHandle,
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
