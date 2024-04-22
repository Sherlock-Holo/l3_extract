use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
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

#[derive(Debug)]
struct AtomicOption<T> {
    used: AtomicBool,
    value: ManuallyDrop<T>,
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

impl<T> Drop for AtomicOption<T> {
    fn drop(&mut self) {
        if !self.used.load(Ordering::Acquire) {
            // safety: atomic bool make sure only drop when no one use
            unsafe {
                ptr::drop_in_place(&mut *self.value);
            }
        }
    }
}
