//! TCP utility types

use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use compio_buf::{IntoInner, IoBuf, IoBufMut};
use crossbeam_channel::SendError;
use derivative::Derivative;
use flume::r#async::RecvStream;
use futures_util::{Stream, StreamExt};
use smoltcp::iface::SocketHandle;

use super::event::OperationEvent;
use super::{cast_dyn_io_buf, cast_dyn_io_buf_mut, TcpInfo, TypedSocketHandle};
use crate::notify_channel::NotifySender;
use crate::shared_buf::{AsIoBuf, AsIoBufMut, SharedBuf};

/// A TCP stream, like tokio/async-net TcpStream
///
/// This TCP stream doesn't like normal [`std::net::TcpStream`] which accepted by
/// [`std::net::TcpListener`], it is a **client** side TCP stream
#[derive(Debug)]
pub struct TcpStream {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    handle: SocketHandle,
    operation_event_tx: NotifySender<OperationEvent>,
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

    /// Pull some bytes from this [`TcpStream`] into the specified buffer, returning how many bytes
    /// were read and the buffer itself.
    pub async fn read<T: IoBufMut + Send + Sync>(&self, buf: T) -> (io::Result<usize>, T) {
        let buf = Arc::new(SharedBuf::new(buf)) as Arc<dyn AsIoBufMut>;
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Read {
            handle: self.handle,
            buffer: buf.clone(),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Read { buffer, .. } => {
                    // make sure buffer ref count is 1
                    drop(buf);

                    // Safety: type is correct
                    let buffer = unsafe { cast_dyn_io_buf_mut(buffer) };

                    return (
                        Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, send read operation event failed",
                        )),
                        buffer,
                    );
                }

                _ => unreachable!(),
            }
        }

        match rx.recv_async().await {
            Err(_) => {
                // Safety: type is correct, and when recv_async failed, means sender is dropped,
                // there is only one reason cause it: TcpStack should own the sender, but TcpStack
                // is dropped
                let buf = unsafe { cast_dyn_io_buf_mut(buf) };

                (
                    Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "TcpStack may be dropped, receive read operation response failed",
                    )),
                    buf,
                )
            }

            Ok(res) => {
                // Safety: type is correct
                let buf = unsafe { cast_dyn_io_buf_mut(buf) };

                (res, buf)
            }
        }
    }

    pub async fn read_exact<T: IoBufMut + Send + Sync>(&self, mut buf: T) -> (io::Result<()>, T) {
        let mut read = 0;
        let len = buf.buf_capacity();

        while read < len {
            let (res, slice) = self.read(buf.slice(read..)).await;
            match res {
                Err(err) => return (Err(err), slice.into_inner()),
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        )),
                        slice.into_inner(),
                    );
                }
                Ok(n) => {
                    buf = slice.into_inner();
                    read += n;
                }
            }
        }

        (Ok(()), buf)
    }

    /// Write a buffer into this [`TcpStream`], returning how many bytes were written and buffer
    /// itself.
    pub async fn write<T: IoBuf + Send + Sync>(&self, buf: T) -> (io::Result<usize>, T) {
        let buf = Arc::new(SharedBuf::new(buf)) as Arc<dyn AsIoBuf>;
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Write {
            handle: self.handle,
            buffer: buf.clone(),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Write { buffer, .. } => {
                    // make sure buffer ref count is 1
                    drop(buf);

                    // Safety: type is correct
                    let buffer = unsafe { cast_dyn_io_buf(buffer) };

                    return (
                        Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, send write operation event failed",
                        )),
                        buffer,
                    );
                }

                _ => unreachable!(),
            }
        }

        match rx.recv_async().await {
            Err(_) => {
                // Safety: type is correct, and when recv_async failed, means sender is dropped,
                // there is only one reason cause it: TcpStack should own the sender, but TcpStack
                // is dropped
                let buf = unsafe { cast_dyn_io_buf(buf) };

                (
                    Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "TcpStack may be dropped, receive write operation response failed",
                    )),
                    buf,
                )
            }

            Ok(res) => {
                // Safety: type is correct
                let buf = unsafe { cast_dyn_io_buf(buf) };

                (res, buf)
            }
        }
    }

    pub async fn write_all<T: IoBuf + Send + Sync>(&self, mut buf: T) -> (io::Result<()>, T) {
        let mut written = 0;
        let len = buf.buf_len();

        while written < len {
            let (res, slice) = self.write(buf.slice(written..)).await;
            match res {
                Err(err) => return (Err(err), slice.into_inner()),
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            ErrorKind::WriteZero,
                            "failed to write whole buffer",
                        )),
                        slice.into_inner(),
                    );
                }
                Ok(n) => {
                    written += n;
                    buf = slice.into_inner();
                }
            }
        }

        (Ok(()), buf)
    }

    /// Shuts down the write halves of this connection.
    pub async fn shutdown(&self) -> io::Result<()> {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Shutdown {
            handle: TypedSocketHandle::Tcp(self.handle),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Shutdown { .. } => {
                    return Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "TcpStack may be dropped, send shutdown operation event failed",
                    ));
                }

                _ => unreachable!(),
            }
        }

        match rx.recv_async().await {
            Err(_) => Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "TcpStack may be dropped, receive shutdown operation response failed",
            )),

            Ok(res) => res,
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = self
            .operation_event_tx
            .send(OperationEvent::Close(TypedSocketHandle::Tcp(self.handle)));
    }
}

/// A TCP socket acceptor, like [`std::net::TcpListener`], but you will accept a client side TCP
/// stream, not server side
#[derive(Derivative)]
#[derivative(Debug)]
pub struct TcpAcceptor {
    #[derivative(Debug = "ignore")]
    pub(crate) tcp_stream_rx: RecvStream<'static, io::Result<TcpInfo>>,
    pub(crate) operation_event_tx: NotifySender<OperationEvent>,
}

impl Stream for TcpAcceptor {
    type Item = io::Result<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let res = ready!(self.tcp_stream_rx.poll_next_unpin(cx)).transpose()?;
        match res {
            None => Poll::Ready(None),
            Some(tcp_info) => Poll::Ready(Some(Ok(TcpStream {
                local_addr: tcp_info.local_addr,
                remote_addr: tcp_info.remote_addr,
                handle: tcp_info.handle,
                operation_event_tx: self.operation_event_tx.clone(),
            }))),
        }
    }
}
