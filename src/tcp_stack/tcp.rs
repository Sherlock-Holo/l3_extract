//! TCP utility types

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::{io, mem};

use bytes::{Buf, BytesMut};
use crossbeam_channel::{Receiver, TryRecvError};
use derivative::Derivative;
use flume::r#async::RecvStream;
use futures_util::task::AtomicWaker;
use futures_util::{AsyncBufRead, AsyncRead, AsyncWrite, Stream, StreamExt};
use smoltcp::iface::SocketHandle;
use tracing::error;

use super::wake_event::{
    CloseEvent, ReadPoll, ReadWakeEvent, ShutdownEvent, WakeEvent, WritePoll, WriteWakeEvent,
};
use super::{create_share_waker, TcpInfo, TypedSocketHandle};
use crate::notify_channel::NotifySender;

#[derive(Derivative)]
#[derivative(Debug)]
enum ReadState {
    Buffer(#[derivative(Debug = "ignore")] BytesMut),
    WaitResult(Receiver<ReadPoll>),
}

#[derive(Derivative)]
#[derivative(Debug)]
enum WriteState {
    Buffer(#[derivative(Debug = "ignore")] BytesMut),
    Flushing(Receiver<WritePoll>),
}

#[derive(Debug)]
enum CloseState {
    Opened,
    Closing(Receiver<Poll<io::Result<()>>>),
    Closed,
}

/// A TCP stream, like tokio/async-net TcpStream
///
/// This TCP stream doesn't like normal [`std::net::TcpStream`] which accepted by
/// [`std::net::TcpListener`], it is a **client** side TCP stream
///
/// ## Notes
///
/// You must call [flush](futures_util::AsyncWriteExt::flush) to make sure data is written to peer
#[derive(Debug)]
pub struct TcpStream {
    read_eof: bool,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    handle: SocketHandle,
    wake_event_tx: NotifySender<WakeEvent>,

    read_buf_cap: usize,
    read_state: ReadState,
    read_waker: Arc<AtomicWaker>,

    write_buf_cap: usize,
    write_state: WriteState,
    write_waker: Arc<AtomicWaker>,

    close_state: CloseState,
    close_waker: Arc<AtomicWaker>,
}

impl TcpStream {
    /// Get local socket addr
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Get peer socket addr
    pub fn peer_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    fn inner_poll_fill_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        if self.read_eof && matches!(self.close_state, CloseState::Closed) {
            return Poll::Ready(Ok(&[]));
        }

        match &self.read_state {
            ReadState::Buffer(buf) => {
                if !buf.is_empty() {
                    // Safety: without the mem::transmute, it can pass polonius borrow checker but
                    // fail with NLL
                    let buf = unsafe { mem::transmute::<&[u8], &[u8]>(buf.as_ref()) };

                    return Poll::Ready(Ok(buf));
                }

                let (tx, rx) = crossbeam_channel::bounded(1);
                let read_state = mem::replace(&mut self.read_state, ReadState::WaitResult(rx));
                let buf = match read_state {
                    ReadState::Buffer(buf) => buf,
                    _ => unreachable!(),
                };

                self.read_waker.register(cx.waker());
                if self
                    .wake_event_tx
                    .send(WakeEvent::Read(ReadWakeEvent {
                        handle: TypedSocketHandle::Tcp(self.handle),
                        buffer: buf,
                        waker: create_share_waker(self.read_waker.clone()),
                        respond: tx,
                    }))
                    .is_err()
                {
                    error!("TcpStack may be dropped, send wake event failed");

                    return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
                }

                Poll::Pending
            }

            ReadState::WaitResult(rx) => {
                self.read_waker.register(cx.waker());

                let read_poll = match rx.try_recv() {
                    Err(TryRecvError::Empty) => return Poll::Pending,
                    Err(TryRecvError::Disconnected) => {
                        error!("TcpStack may be dropped, try receive wake event response failed");

                        return Poll::Ready(Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, try receive wake event response failed",
                        )));
                    }
                    Ok(read_poll) => read_poll,
                };

                match read_poll {
                    ReadPoll::Pending(buf) => {
                        self.read_state = ReadState::Buffer(buf);

                        // tcp stack wake but in earlier tcp socket not ready, so we need to poll
                        // fill buf again
                        self.inner_poll_fill_buf(cx)
                    }

                    ReadPoll::Ready { buf, result } => {
                        let eof = buf.is_empty();
                        self.read_state = ReadState::Buffer(buf);

                        result?;
                        self.read_eof = eof;

                        self.inner_poll_fill_buf(cx)
                    }
                }
            }
        }
    }

    fn is_close_write(&self) -> bool {
        matches!(
            self.close_state,
            CloseState::Closed | CloseState::Closing(_)
        )
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let inner_buf = ready!(self.as_mut().poll_fill_buf(cx))?;
        if inner_buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let n = inner_buf.len().min(buf.len());
        buf[..n].copy_from_slice(&inner_buf[..n]);
        self.consume(n);

        Poll::Ready(Ok(n))
    }
}

impl AsyncBufRead for TcpStream {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.get_mut().inner_poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        let cap = self.read_buf_cap;
        match &mut self.read_state {
            ReadState::Buffer(buf) => {
                buf.advance(amt);
                if !buf.has_remaining() {
                    buf.reserve(cap);
                }
            }
            ReadState::WaitResult(_) => {
                panic!("TcpStream is not ready for read")
            }
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_close_write() {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "TcpStream is closed",
            )));
        }

        match &mut self.write_state {
            WriteState::Buffer(inner_buf) => {
                let available_write_size = inner_buf.capacity() - inner_buf.len();
                let need_flush = available_write_size < buf.len();
                let n = available_write_size.min(buf.len());
                inner_buf.extend_from_slice(&buf[..n]);

                if !need_flush {
                    return Poll::Ready(Ok(n));
                }

                match self.as_mut().poll_flush(cx) {
                    Poll::Pending => {
                        if n > 0 {
                            Poll::Ready(Ok(n))
                        } else {
                            Poll::Pending
                        }
                    }

                    Poll::Ready(res) => {
                        res?;

                        if n > 0 {
                            Poll::Ready(Ok(n))
                        } else {
                            self.poll_write(cx, buf)
                        }
                    }
                }
            }

            WriteState::Flushing(_) => {
                ready!(self.as_mut().poll_flush(cx))?;

                self.poll_write(cx, buf)
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_close_write() {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "TcpStream is closed",
            )));
        }

        match &self.write_state {
            WriteState::Flushing(rx) => {
                self.write_waker.register(cx.waker());

                let write_poll = match rx.try_recv() {
                    Err(TryRecvError::Empty) => return Poll::Pending,
                    Err(TryRecvError::Disconnected) => {
                        error!("TcpStack may be dropped, try receive wake event response failed");

                        return Poll::Ready(Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, try receive wake event response failed",
                        )));
                    }
                    Ok(write_poll) => write_poll,
                };

                match write_poll {
                    WritePoll::Pending(buf) => {
                        self.write_state = WriteState::Buffer(buf);

                        // tcp stack wake but in earlier tcp socket not ready, so we need to poll
                        // flush again
                        self.poll_flush(cx)
                    }

                    WritePoll::Ready { mut buf, result } => {
                        let has_remaining = buf.has_remaining();
                        if !has_remaining {
                            buf.reserve(self.write_buf_cap);
                        }
                        self.write_state = WriteState::Buffer(buf);
                        result?;

                        // still need continue flush to write all buffer data
                        if has_remaining {
                            self.poll_flush(cx)
                        } else {
                            Poll::Ready(Ok(()))
                        }
                    }
                }
            }

            WriteState::Buffer(buf) => {
                if buf.is_empty() {
                    return Poll::Ready(Ok(()));
                }

                let (tx, rx) = crossbeam_channel::bounded(1);
                let write_state = mem::replace(&mut self.write_state, WriteState::Flushing(rx));
                let buf = match write_state {
                    WriteState::Buffer(buf) => buf,
                    _ => unreachable!(),
                };

                self.write_waker.register(cx.waker());
                if self
                    .wake_event_tx
                    .send(WakeEvent::Write(WriteWakeEvent {
                        handle: TypedSocketHandle::Tcp(self.handle),
                        data: buf,
                        waker: create_share_waker(self.write_waker.clone()),
                        respond: tx,
                    }))
                    .is_err()
                {
                    error!("TcpStack may be dropped, send wake event failed");

                    return Poll::Ready(Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "TcpStack may be dropped, send wake event failed",
                    )));
                }

                Poll::Pending
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &self.close_state {
            CloseState::Opened => {
                let (tx, rx) = crossbeam_channel::bounded(1);
                self.close_waker.register(cx.waker());
                if self
                    .wake_event_tx
                    .send(WakeEvent::Shutdown(ShutdownEvent {
                        handle: TypedSocketHandle::Tcp(self.handle),
                        waker: create_share_waker(self.close_waker.clone()),
                        respond: tx,
                    }))
                    .is_err()
                {
                    error!("TcpStack may be dropped, send wake event failed");

                    return Poll::Ready(Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "TcpStack may be dropped, send wake event failed",
                    )));
                }

                self.close_state = CloseState::Closing(rx);

                Poll::Pending
            }

            CloseState::Closing(rx) => {
                self.close_waker.register(cx.waker());

                match rx.try_recv() {
                    Err(TryRecvError::Empty) => Poll::Pending,
                    Err(TryRecvError::Disconnected) => {
                        error!("TcpStack may be dropped, try receive wake event response failed");

                        Poll::Ready(Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, try receive wake event response failed",
                        )))
                    }

                    Ok(poll) => match poll {
                        Poll::Ready(res) => {
                            self.close_state = CloseState::Closed;

                            Poll::Ready(res)
                        }

                        Poll::Pending => {
                            // tcp stack wake but in earlier tcp socket not ready, so we need
                            // to poll close again
                            self.poll_close(cx)
                        }
                    },
                }
            }

            CloseState::Closed => Poll::Ready(Ok(())),
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = self.wake_event_tx.send(WakeEvent::Close(CloseEvent {
            handle: TypedSocketHandle::Tcp(self.handle),
        }));
    }
}

/// a TCP socket acceptor, like [`std::net::TcpListener`], but you will accept a client side TCP
/// stream, not server side
#[derive(Derivative)]
#[derivative(Debug)]
pub struct TcpAcceptor {
    #[derivative(Debug = "ignore")]
    pub(crate) tcp_stream_rx: RecvStream<'static, io::Result<TcpInfo>>,
    pub(crate) wake_event_tx: NotifySender<WakeEvent>,
    pub(crate) buf_cap: usize,
}

impl Stream for TcpAcceptor {
    type Item = io::Result<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let res = ready!(self.tcp_stream_rx.poll_next_unpin(cx)).transpose()?;
        match res {
            None => Poll::Ready(None),
            Some(tcp_info) => Poll::Ready(Some(Ok(TcpStream {
                read_eof: false,
                local_addr: tcp_info.local_addr,
                remote_addr: tcp_info.remote_addr,
                handle: tcp_info.handle,
                wake_event_tx: self.wake_event_tx.clone(),
                read_buf_cap: self.buf_cap,
                read_state: ReadState::Buffer(BytesMut::with_capacity(self.buf_cap)),
                read_waker: Default::default(),
                write_buf_cap: self.buf_cap,
                write_state: WriteState::Buffer(BytesMut::with_capacity(self.buf_cap)),
                write_waker: Arc::new(Default::default()),
                close_state: CloseState::Opened,
                close_waker: Arc::new(Default::default()),
            }))),
        }
    }
}
