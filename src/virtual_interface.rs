use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken};
use smoltcp::time::Instant;

#[derive(Debug)]
pub struct VirtualInterface {
    cap: DeviceCapabilities,
    tx_queue: VecDeque<Bytes>,
    rx_queue: VecDeque<BytesMut>,
}

impl VirtualInterface {
    pub fn new(cap: DeviceCapabilities) -> Self {
        Self {
            cap,
            tx_queue: Default::default(),
            rx_queue: Default::default(),
        }
    }

    pub fn push_receive_packet(&mut self, packet: BytesMut) {
        self.rx_queue.push_back(packet);
    }

    pub fn pop_all_send_packets(&mut self) -> impl IntoIterator<Item = Bytes> + '_ {
        self.tx_queue.drain(..)
    }
}

impl Device for VirtualInterface {
    type RxToken<'a> = VirtualRxToken<'a> where Self: 'a;
    type TxToken<'a> = VirtualTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_queue.is_empty() {
            return None;
        }

        Some((
            VirtualRxToken {
                queue: &mut self.rx_queue,
            },
            VirtualTxToken {
                queue: &mut self.tx_queue,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtualTxToken {
            queue: &mut self.tx_queue,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.cap.clone()
    }
}

#[derive(Debug)]
pub struct VirtualRxToken<'a> {
    queue: &'a mut VecDeque<BytesMut>,
}

impl<'a> RxToken for VirtualRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = self.queue.pop_front().unwrap();

        f(&mut buf)
    }
}

#[derive(Debug)]
pub struct VirtualTxToken<'a> {
    queue: &'a mut VecDeque<Bytes>,
}

impl<'a> TxToken for VirtualTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = BytesMut::zeroed(len);
        let res = f(&mut buf);
        self.queue.push_back(buf.freeze());

        res
    }
}
