//! Timer trait.

use std::time::Duration;

/// Timer trait.
///
/// [TcpStack] will use the timer to sleep.
///
/// [TcpStack]: crate::TcpStack
#[allow(async_fn_in_trait)]
pub trait Timer {
    async fn sleep(&mut self, dur: Duration);
}