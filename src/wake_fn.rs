use std::sync::Arc;
use std::task::{Wake, Waker};

pub fn wake_fn<F: Fn() + Send + Sync + 'static>(f: F) -> Waker {
    Waker::from(Arc::new(WakeFn { f }))
}

#[derive(Debug)]
struct WakeFn<F: Fn()> {
    f: F,
}

impl<F: Fn()> Wake for WakeFn<F> {
    fn wake(self: Arc<Self>) {
        (self.f)();
    }
}
