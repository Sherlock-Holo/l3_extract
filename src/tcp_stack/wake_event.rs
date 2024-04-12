use std::io;
use std::task::{Poll, Waker};

use bytes::BytesMut;
use crossbeam_channel::Sender;
use derivative::Derivative;

use super::TypedSocketHandle;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct WriteWakeEvent {
    pub handle: TypedSocketHandle,
    #[derivative(Debug = "ignore")]
    pub data: BytesMut,
    pub waker: Waker,
    pub respond: Sender<WritePoll>,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct ReadWakeEvent {
    pub handle: TypedSocketHandle,
    #[derivative(Debug = "ignore")]
    pub buffer: BytesMut,
    pub waker: Waker,
    pub respond: Sender<ReadPoll>,
}

pub enum ReadPoll {
    Pending(BytesMut),
    Ready {
        buf: BytesMut,
        result: io::Result<()>,
    },
}

pub enum WritePoll {
    Pending(BytesMut),
    Ready {
        buf: BytesMut,
        result: io::Result<()>,
    },
}

#[derive(Debug)]
pub struct ReadyEvent {
    pub handle: TypedSocketHandle,
}

#[derive(Debug)]
pub struct CloseEvent {
    pub handle: TypedSocketHandle,
    pub waker: Waker,
    pub respond: Sender<Poll<io::Result<()>>>,
}

#[derive(Debug)]
pub enum WakeEvent {
    Write(WriteWakeEvent),
    Read(ReadWakeEvent),
    Ready(ReadyEvent),
    Close(CloseEvent),
}
