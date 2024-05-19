//! UDP utility types.

use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use compio_buf::{IoBuf, IoBufMut};
use crossbeam_channel::SendError;
use derivative::Derivative;
use flume::r#async::RecvStream;
use futures_util::{Stream, StreamExt};
use smoltcp::iface::SocketHandle;

use super::event::OperationEvent;
use super::{cast_dyn_io_buf, cast_dyn_io_buf_mut, TypedSocketHandle, UdpInfo};
use crate::notify_channel::NotifySender;
use crate::shared_buf::{AsIoBuf, AsIoBufMut, SharedBuf};

/// A UDP socket, like tokio/async-net UdpSocket.
///
/// This UDP socket is a **server** side UDP socket, like [`std::net::UdpSocket`].
#[derive(Debug)]
pub struct UdpSocket {
    remote_addr: SocketAddr,
    handle: SocketHandle,
    operation_event_tx: NotifySender<OperationEvent>,
}

impl UdpSocket {
    /// Get peer socket addr.
    pub fn peer_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Receives a single datagram message on the [`UdpSocket`], and return the `buf` and source
    /// `addr`.
    pub async fn recv<T: IoBufMut + Send + Sync>(
        &self,
        buf: T,
    ) -> (io::Result<(usize, SocketAddr)>, T) {
        let buf = Arc::new(SharedBuf::new(buf)) as Arc<dyn AsIoBufMut>;
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Recv {
            handle: self.handle,
            buffer: buf.clone(),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Recv { buffer, .. } => {
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

    /// Sends data to specify `addr` on the [`UdpSocket`].
    pub async fn send<T: IoBuf + Send + Sync>(
        &self,
        buf: T,
        addr: SocketAddr,
    ) -> (io::Result<usize>, T) {
        let buf = Arc::new(SharedBuf::new(buf)) as Arc<dyn AsIoBuf>;
        let (tx, rx) = flume::bounded(1);
        if let Err(SendError(event)) = self.operation_event_tx.send(OperationEvent::Send {
            handle: self.handle,
            src: addr,
            buffer: buf.clone(),
            result_tx: tx,
        }) {
            match event {
                OperationEvent::Send { buffer, .. } => {
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
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = self
            .operation_event_tx
            .send(OperationEvent::Close(TypedSocketHandle::Udp(self.handle)));
    }
}

/// a UDP socket acceptor, like [TcpAcceptor](super::tcp::TcpAcceptor), but you will accept a
/// client side UDP socket, not server side.
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
