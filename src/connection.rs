//! Layer 3 connection.

use std::io;

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
    async fn consume(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Sink a layer 3 packet to the connection.
    async fn sink(&mut self, packet: &[u8]) -> io::Result<()>;
}

#[cfg(feature = "futures_io_wrapper")]
mod futures_io_wrapper {
    use std::io;

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
        async fn consume(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf).await
        }

        #[inline]
        async fn sink(&mut self, packet: &[u8]) -> io::Result<()> {
            self.0.write(packet).await.map(|_| ())
        }
    }
}
