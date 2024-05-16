//! TCP utility types

use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use compio_buf::{IoBuf, IoBufMut};
use crossbeam_channel::SendError;
use derivative::Derivative;
use flume::r#async::RecvStream;
use futures_util::{Stream, StreamExt};
use smoltcp::iface::SocketHandle;

use super::event::OperationEvent;
use super::{cast_dyn_io_buf, cast_dyn_io_buf_mut, TcpInfo, TypedSocketHandle};
use crate::notify_channel::NotifySender;

/// A TCP stream, like tokio/async-net TcpStream
///
/// This TCP stream doesn't like normal [`std::net::TcpStream`] which accepted by
/// [`std::net::TcpListener`], it is a **client** side TCP stream
#[derive(Debug)]
pub struct TcpStream {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    handle: SocketHandle,
    read_part: ReadPart,
    write_part: WritePart,
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
    pub async fn read<T: IoBufMut + Send + 'static>(
        &self,
        buf: T,
    ) -> (io::Result<usize>, Option<T>) {
        self.read_part.read(buf, self.handle).await
    }

    /// Write a buffer into this [`TcpStream`], returning how many bytes were written and buffer
    /// itself.
    pub async fn write<T: IoBuf + Send + 'static>(&self, buf: T) -> (io::Result<usize>, Option<T>) {
        self.write_part.write(buf, self.handle).await
    }

    /// Shuts down the write halves of this connection.
    pub async fn shutdown(&self) -> io::Result<()> {
        self.write_part.shutdown(self.handle).await
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = self
            .write_part
            .operation_event_tx
            .send(OperationEvent::Close(TypedSocketHandle::Tcp(self.handle)));
    }
}

#[derive(Debug)]
struct WritePart {
    operation_event_tx: NotifySender<OperationEvent>,
}

impl WritePart {
    async fn write<T: IoBuf + Send + 'static>(
        &self,
        buf: T,
        handle: SocketHandle,
    ) -> (io::Result<usize>, Option<T>) {
        if buf.buf_len() == 0 {
            return (Ok(0), Some(buf));
        }

        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Write {
            handle,
            buffer: Box::new(buf),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Write { buffer, .. } => {
                    // Safety: type is correct
                    let buffer = unsafe { cast_dyn_io_buf(buffer) };

                    return (
                        Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, send write operation event failed",
                        )),
                        Some(buffer),
                    );
                }

                _ => unreachable!(),
            }
        }

        match rx.recv_async().await {
            Err(_) => (
                Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "TcpStack may be dropped, receive write operation response failed",
                )),
                None,
            ),

            Ok((res, buf)) => {
                // Safety: type is correct
                let buf = unsafe { cast_dyn_io_buf(buf) };

                (res, Some(buf))
            }
        }
    }

    async fn shutdown(&self, handle: SocketHandle) -> io::Result<()> {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Shutdown {
            handle: TypedSocketHandle::Tcp(handle),
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

#[derive(Debug)]
struct ReadPart {
    operation_event_tx: NotifySender<OperationEvent>,
}

impl ReadPart {
    async fn read<T: IoBufMut + Send + 'static>(
        &self,
        buf: T,
        handle: SocketHandle,
    ) -> (io::Result<usize>, Option<T>) {
        if buf.buf_capacity() - buf.buf_len() == 0 {
            return (Ok(0), Some(buf));
        }

        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Read {
            handle,
            buffer: Box::new(buf),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Read { buffer, .. } => {
                    // Safety: type is correct
                    let buffer = unsafe { cast_dyn_io_buf_mut(buffer) };

                    return (
                        Err(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "TcpStack may be dropped, send read operation event failed",
                        )),
                        Some(buffer),
                    );
                }

                _ => unreachable!(),
            }
        }

        match rx.recv_async().await {
            Err(_) => (
                Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "TcpStack may be dropped, receive read operation response failed",
                )),
                None,
            ),

            Ok((res, buf)) => {
                // Safety: type is correct
                let buf = unsafe { cast_dyn_io_buf_mut(buf) };

                (res, Some(buf))
            }
        }
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
                read_part: ReadPart {
                    operation_event_tx: self.operation_event_tx.clone(),
                },
                write_part: WritePart {
                    operation_event_tx: self.operation_event_tx.clone(),
                },
            }))),
        }
    }
}
