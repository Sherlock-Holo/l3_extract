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

use super::event::OperationEvent;
use super::{TypedSocketHandle, UdpInfo};
use crate::notify_channel::NotifySender;

/// A UDP socket, like tokio/async-net UdpSocket
///
/// This UDP socket is a **client** side UDP socket, like [`std::net::UdpSocket`] which has called
/// [connect](std::net::UdpSocket::connect)
#[derive(Debug)]
pub struct UdpSocket {
    remote_addr: SocketAddr,
    handle: SocketHandle,
    operation_event_tx: NotifySender<OperationEvent>,
}

impl UdpSocket {
    /// Get peer socket addr
    pub fn peer_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Receives a single datagram message on the [`UdpSocket`]
    pub async fn recv(&self, buf: Vec<u8>) -> (io::Result<(usize, SocketAddr)>, Option<Vec<u8>>) {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Recv {
            handle: self.handle,
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

    /// Sends data on the [`UdpSocket`]
    pub async fn send(
        &self,
        buf: Vec<u8>,
        addr: SocketAddr,
    ) -> (io::Result<usize>, Option<Vec<u8>>) {
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Send {
            handle: self.handle,
            src: addr,
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

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = self
            .operation_event_tx
            .send(OperationEvent::Close(TypedSocketHandle::Udp(self.handle)));
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
                remote_addr: udp_info.remote_addr,
                handle: udp_info.handle,
                operation_event_tx: self.operation_event_tx.clone(),
            }))),
        }
    }
}
