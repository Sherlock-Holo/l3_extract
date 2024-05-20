//! Layer 3 connection.

use std::io;

use compio_buf::{IoBuf, IoBufMut};

#[cfg(feature = "futures_io_wrapper")]
#[doc(inline)]
pub use self::futures_io_wrapper::FuturesIoWrapper;

/// Layer 3 connection.
///
/// [TcpStack] will consume and sink layer 3 Packet through the connection.
///
/// [TcpStack]: crate::TcpStack
#[allow(async_fn_in_trait)]
pub trait Connection {
    /// Consume a layer 3 packet from the connection.
    ///
    /// # Notes:
    ///
    /// this method **MUST** be cancelable.
    async fn consume<B: IoBufMut>(&mut self, buf: B) -> (io::Result<usize>, B);

    /// Sink a layer 3 packet to the connection.
    async fn sink<B: IoBuf>(&mut self, packet: B) -> (io::Result<()>, B);
}

#[cfg(feature = "futures_io_wrapper")]
mod futures_io_wrapper {
    use std::{io, slice};

    use compio_buf::{IoBuf, IoBufMut};
    use futures_util::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::Connection;

    /// A futures-io [`AsyncRead`], [`AsyncWrite`] wrapper, implement the [`Connection`] trait.
    #[derive(Debug)]
    pub struct FuturesIoWrapper<T>(pub T);

    impl<T> Connection for FuturesIoWrapper<T>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        #[inline]
        async fn consume<B: IoBufMut>(&mut self, mut buf: B) -> (io::Result<usize>, B) {
            let uninitiated_buf = buf.as_mut_slice();
            // Safety: most AsyncRead implement won't read the buffer
            let uninitiated_buf = unsafe {
                slice::from_raw_parts_mut(
                    uninitiated_buf.as_mut_ptr().cast(),
                    uninitiated_buf.len(),
                )
            };

            let res = self.0.read(uninitiated_buf).await;
            if let Ok(n) = res {
                // Safety: we have written n bytes
                unsafe { buf.set_buf_init(n) }
            }

            (res, buf)
        }

        #[inline]
        async fn sink<B: IoBuf>(&mut self, packet: B) -> (io::Result<()>, B) {
            let res = self.0.write(packet.as_slice()).await;
            (res.map(|_| ()), packet)
        }
    }
}
