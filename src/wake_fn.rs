use std::fmt::{Debug, Formatter};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Wake, Waker};

pub fn wake_once_fn<F: FnOnce() + Send + Sync + 'static>(f: F) -> Waker {
    Waker::from(Arc::new(WakeOnceFn {
        f: AtomicOption {
            used: Default::default(),
            value: ManuallyDrop::new(f),
        },
    }))
}

#[derive(Debug)]
struct WakeOnceFn<F: FnOnce()> {
    f: AtomicOption<F>,
}

impl<F: FnOnce()> Wake for WakeOnceFn<F> {
    fn wake(self: Arc<Self>) {
        if let Some(f) = self.f.take() {
            f()
        }
    }
}

struct AtomicOption<T> {
    used: AtomicBool,
    value: ManuallyDrop<T>,
}

impl<T> Debug for AtomicOption<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtomicOption")
            .field("used", &self.used.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl<T> AtomicOption<T> {
    pub fn take(&self) -> Option<T> {
        if self
            .used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // safety: atomic bool make sure only read once
            unsafe { Some(ptr::read(&*self.value)) }
        } else {
            None
        }
    }
}

unsafe impl<T: Send> Sync for AtomicOption<T> {}

impl<T> Drop for AtomicOption<T> {
    fn drop(&mut self) {
        if !self.used.load(Ordering::Acquire) {
            // safety: atomic bool make sure only drop when no one use
            unsafe {
                ManuallyDrop::drop(&mut self.value);
            }
        }
    }
}
