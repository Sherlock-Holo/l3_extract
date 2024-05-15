//! TCP utility types

use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use crossbeam_channel::SendError;
use derivative::Derivative;
use flume::r#async::RecvStream;
use futures_util::{Stream, StreamExt};
use smoltcp::iface::SocketHandle;

use super::wake_event::OperationEvent;
use super::{TcpInfo, TypedSocketHandle};
use crate::notify_channel::NotifySender;

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

    pub async fn read(&self, buf: Vec<u8>) -> (io::Result<usize>, Option<Vec<u8>>) {
        self.read_part.read(buf, self.handle).await
    }

    pub async fn write(&self, buf: Vec<u8>) -> (io::Result<usize>, Option<Vec<u8>>) {
        self.write_part.write(buf, self.handle).await
    }

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
    is_shutdown: bool,
    operation_event_tx: NotifySender<OperationEvent>,
}

impl WritePart {
    async fn write(
        &self,
        buf: Vec<u8>,
        handle: SocketHandle,
    ) -> (io::Result<usize>, Option<Vec<u8>>) {
        if self.is_shutdown {
            return (
                Err(io::Error::new(ErrorKind::BrokenPipe, "TcpStream is closed")),
                Some(buf),
            );
        }
        if buf.is_empty() {
            return (Ok(0), Some(buf));
        }

        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Write {
            handle: TypedSocketHandle::Tcp(handle),
            buffer: buf,
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Write { buffer, .. } => {
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

            Ok((res, buf)) => (res, Some(buf)),
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
    read_eof: bool,
    operation_event_tx: NotifySender<OperationEvent>,
}

impl ReadPart {
    async fn read(
        &self,
        buf: Vec<u8>,
        handle: SocketHandle,
    ) -> (io::Result<usize>, Option<Vec<u8>>) {
        if self.read_eof {
            return (
                Err(io::Error::new(ErrorKind::BrokenPipe, "TcpStream is closed")),
                Some(buf),
            );
        }
        if buf.is_empty() {
            return (Ok(0), Some(buf));
        }

        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Read {
            handle: TypedSocketHandle::Tcp(handle),
            buffer: buf,
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Read { buffer, .. } => {
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

            Ok((res, buf)) => (res, Some(buf)),
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
                    read_eof: false,
                    operation_event_tx: self.operation_event_tx.clone(),
                },
                write_part: WritePart {
                    is_shutdown: false,
                    operation_event_tx: self.operation_event_tx.clone(),
                },
            }))),
        }
    }
}