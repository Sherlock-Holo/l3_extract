//! UDP utility types

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
use super::{TypedSocketHandle, UdpInfo};
use crate::notify_channel::NotifySender;

/// A UDP socket, like tokio/async-net UdpSocket
///
/// This UDP socket is a **client** side UDP socket, like [`std::net::UdpSocket`] which has called
/// [connect](std::net::UdpSocket::connect)
#[derive(Debug)]
pub struct UdpSocket {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    handle: SocketHandle,
    read_part: ReadPart,
    write_part: WritePart,
}

impl UdpSocket {
    /// Get local socket addr
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Get peer socket addr
    pub fn peer_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    pub async fn recv(&self, buf: Vec<u8>) -> (io::Result<usize>, Option<Vec<u8>>) {
        self.read_part
            .read(buf, self.handle, self.local_addr, self.remote_addr)
            .await
    }

    pub async fn send(&self, buf: Vec<u8>) -> (io::Result<usize>, Option<Vec<u8>>) {
        self.write_part
            .write(buf, self.handle, self.local_addr, self.remote_addr)
            .await
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = self
            .write_part
            .operation_event_tx
            .send(OperationEvent::Close(TypedSocketHandle::Udp {
                handle: self.handle,
                src: self.local_addr,
                dst: self.remote_addr,
            }));
    }
}

#[derive(Debug)]
struct WritePart {
    operation_event_tx: NotifySender<OperationEvent>,
}

impl WritePart {
    async fn write(
        &self,
        buf: Vec<u8>,
        handle: SocketHandle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (io::Result<usize>, Option<Vec<u8>>) {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Write {
            handle: TypedSocketHandle::Udp {
                handle,
                src: local_addr,
                dst: remote_addr,
            },
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
}

#[derive(Debug)]
struct ReadPart {
    operation_event_tx: NotifySender<OperationEvent>,
}

impl ReadPart {
    async fn read(
        &self,
        buf: Vec<u8>,
        handle: SocketHandle,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> (io::Result<usize>, Option<Vec<u8>>) {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Read {
            handle: TypedSocketHandle::Udp {
                handle,
                src: local_addr,
                dst: remote_addr,
            },
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

/// a UDP socket acceptor, like [TcpAcceptor](super::tcp::TcpAcceptor), but you will accept a
/// client side UDP socket, not server side
#[derive(Derivative)]
#[derivative(Debug)]
pub struct UdpAcceptor {
    #[derivative(Debug = "ignore")]
    pub(crate) udp_stream_rx: RecvStream<'static, io::Result<UdpInfo>>,
    pub(crate) operation_event_tx: NotifySender<OperationEvent>,
}

impl Stream for UdpAcceptor {
    type Item = io::Result<UdpSocket>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let res = ready!(self.udp_stream_rx.poll_next_unpin(cx)).transpose()?;
        match res {
            None => Poll::Ready(None),
            Some(udp_info) => Poll::Ready(Some(Ok(UdpSocket {
                local_addr: udp_info.local_addr,
                remote_addr: udp_info.remote_addr,
                handle: udp_info.handle,
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
