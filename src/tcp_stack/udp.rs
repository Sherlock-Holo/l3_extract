//! UDP utility types

use std::fmt::{Debug, Formatter};
use std::future::poll_fn;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::{io, mem};

use bytes::{Buf, BytesMut};
use crossbeam_channel::{Receiver, TryRecvError};
use flume::r#async::RecvStream;
use futures_util::lock::Mutex;
use futures_util::task::AtomicWaker;
use futures_util::{Stream, StreamExt};
use smoltcp::iface::SocketHandle;
use tracing::error;

use super::wake_event::{
    CloseEvent, ReadPoll, ReadWakeEvent, WakeEvent, WritePoll, WriteWakeEvent,
};
use super::{create_share_waker, TypedSocketHandle, UdpInfo};
use crate::notify_channel::NotifySender;

enum RecvState {
    Buffer(BytesMut),
    WaitResult(Receiver<ReadPoll>),
}

impl Debug for RecvState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvState::Buffer(_) => f.debug_struct("RecvState::Buffer").finish_non_exhaustive(),

            RecvState::WaitResult(receiver) => f
                .debug_struct("RecvState::WaitResult")
                .field("receiver", receiver)
                .finish(),
        }
    }
}

enum SendState {
    Buffer(BytesMut),
    Sending(Receiver<WritePoll>),
}

impl Debug for SendState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SendState::Buffer(_) => f.debug_struct("SendState::Buffer").finish_non_exhaustive(),
            SendState::Sending(rx) => f
                .debug_struct("SendState::Sending")
                .field("receiver", rx)
                .finish(),
        }
    }
}

/// A UDP socket, like tokio/async-net UdpSocket
///
/// This UDP socket is a **client** side UDP socket, like [`std::net::UdpSocket`] which has called
/// [connect](std::net::UdpSocket::connect)
#[derive(Debug)]
pub struct UdpSocket {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    handle: SocketHandle,
    send_payload_capacity: usize,
    wake_event_tx: NotifySender<WakeEvent>,

    read_state: Mutex<RecvState>,
    read_waker: Arc<AtomicWaker>,

    write_state: Mutex<SendState>,
    write_waker: Arc<AtomicWaker>,
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

    /// Send payload capacity, if send data size is large than that, will return error
    pub fn send_payload_capacity(&self) -> usize {
        self.send_payload_capacity
    }

    /// Receive data with buffer from peer, will return actually receive size
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read_state = self.read_state.lock().await;

        poll_fn(|cx| {
            loop {
                match &*read_state {
                    RecvState::Buffer(_) => {
                        let (tx, rx) = crossbeam_channel::bounded(1);
                        let read_state = mem::replace(&mut *read_state, RecvState::WaitResult(rx));
                        let buf = match read_state {
                            RecvState::Buffer(buf) => buf,
                            _ => unreachable!(),
                        };

                        self.read_waker.register(cx.waker());
                        if self
                            .wake_event_tx
                            .send(WakeEvent::Read(ReadWakeEvent {
                                handle: TypedSocketHandle::Udp { handle: self.handle, src: self.local_addr, dst: self.remote_addr },
                                buffer: buf,
                                waker: create_share_waker(self.read_waker.clone()),
                                respond: tx,
                            }))
                            .is_err()
                        {
                            error!("TcpStack may be dropped, send wake event failed");

                            return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
                        }

                        return Poll::Pending;
                    }

                    RecvState::WaitResult(rx) => {
                        self.read_waker.register(cx.waker());

                        let read_poll = match rx.try_recv() {
                            Err(TryRecvError::Empty) => return Poll::Pending,
                            Err(TryRecvError::Disconnected) => {
                                error!("TcpStack may be dropped, try receive wake event response failed");

                                return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
                            }
                            Ok(read_poll) => read_poll,
                        };

                        match read_poll {
                            ReadPoll::Pending(buf) => {
                                *read_state = RecvState::Buffer(buf);

                                // tcp stack wake but in earlier udp socket not ready, so we need to
                                // poll again
                                continue;
                            }

                            ReadPoll::Ready { buf: mut inner_buf, result } => {
                                return match result {
                                    Ok(_) => {
                                        let n = inner_buf.len().min(buf.len());
                                        inner_buf.copy_to_slice(&mut buf[..n]);
                                        inner_buf.clear();

                                        *read_state = RecvState::Buffer(inner_buf);

                                        Poll::Ready(Ok(n))
                                    }

                                    Err(err) => {
                                        *read_state = RecvState::Buffer(inner_buf);

                                        Poll::Ready(Err(err))
                                    }
                                };
                            }
                        }
                    }
                }
            }
        }).await
    }

    /// Send data to peer, will return sent size
    pub async fn send(&self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.send_payload_capacity {
            return Err(io::Error::new(
                ErrorKind::Other,
                format!(
                    "data size is larger than send payload capacity: {}",
                    self.send_payload_capacity
                ),
            ));
        }

        let mut write_state = self.write_state.lock().await;

        let mut copied = false;
        poll_fn(|cx| {
            loop {
                match &*write_state {
                    SendState::Buffer(_) => {
                        let (tx, rx) = crossbeam_channel::bounded(1);
                        let write_state = mem::replace(&mut *write_state, SendState::Sending(rx));
                        let mut buf = match write_state {
                            SendState::Buffer(buf) => buf,
                            _ => unreachable!(),
                        };

                        // avoid unnecessary copy
                        if !copied {
                            buf.extend_from_slice(data);
                            copied = true;
                        }

                        self.write_waker.register(cx.waker());

                        if self.wake_event_tx.send(WakeEvent::Write(WriteWakeEvent {
                            handle: TypedSocketHandle::Udp { handle: self.handle, src: self.local_addr, dst: self.remote_addr },
                            data: buf,
                            waker: create_share_waker(self.write_waker.clone()),
                            respond: tx,
                        })).is_err() {
                            error!("TcpStack may be dropped, send wake event failed");

                            return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
                        }

                        return Poll::Pending;
                    }

                    SendState::Sending(rx) => {
                        self.write_waker.register(cx.waker());

                        let write_poll = match rx.try_recv() {
                            Err(TryRecvError::Empty) => return Poll::Pending,
                            Err(TryRecvError::Disconnected) => {
                                error!("TcpStack may be dropped, try receive wake event response failed");

                                return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
                            }
                            Ok(write_poll) => write_poll,
                        };

                        match write_poll {
                            WritePoll::Pending(buf) => {
                                *write_state = SendState::Buffer(buf);

                                // tcp stack wake but in earlier udp socket not ready, so we need
                                // to poll again
                                continue;
                            }

                            WritePoll::Ready { buf, result } => {
                                *write_state = SendState::Buffer(buf);

                                return Poll::Ready(result);
                            }
                        }
                    }
                }
            }
        }).await?;

        Ok(data.len())
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = self.wake_event_tx.send(WakeEvent::Close(CloseEvent {
            handle: TypedSocketHandle::Udp {
                handle: self.handle,
                src: self.local_addr,
                dst: self.remote_addr,
            },
        }));
    }
}

/// a UDP socket acceptor, like [TcpAcceptor](super::tcp::TcpAcceptor), but you will accept a
/// client side UDP socket, not server side
pub struct UdpAcceptor {
    pub(crate) udp_stream_rx: RecvStream<'static, io::Result<UdpInfo>>,
    pub(crate) wake_event_tx: NotifySender<WakeEvent>,
}

impl Debug for UdpAcceptor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpAcceptor")
            .field("udp_stream_rx", &"{..}")
            .field("wake_event_tx", &self.wake_event_tx)
            .finish()
    }
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
                send_payload_capacity: udp_info.send_payload_capacity,
                wake_event_tx: self.wake_event_tx.clone(),
                read_state: Mutex::new(RecvState::Buffer(BytesMut::with_capacity(
                    udp_info.recv_payload_capacity,
                ))),
                write_state: Mutex::new(SendState::Buffer(BytesMut::with_capacity(
                    udp_info.send_payload_capacity,
                ))),
                read_waker: Default::default(),
                write_waker: Arc::new(Default::default()),
            }))),
        }
    }
}
