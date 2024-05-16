use std::cell::UnsafeCell;
use std::slice;

use compio_buf::{IoBuf, IoBufMut};

pub struct SharedBuf<T> {
    inner: UnsafeCell<T>,
}

impl<T> SharedBuf<T> {
    pub fn new(buf: T) -> Self {
        Self {
            inner: UnsafeCell::new(buf),
        }
    }

    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

unsafe impl<T: Sync> Sync for SharedBuf<T> {}

pub trait AsIoBuf: Send + Sync {
    /// # Safety:
    ///
    /// user must make sure when use [`IoBuf`], no other one use [`IoBufMut`] or [`SetBufInit`]
    unsafe fn as_slice(&self) -> &[u8];
}

pub trait AsIoBufMut: Send + Sync {
    /// # Safety:
    ///
    /// user must make sure when use [`IoBufMut`], no other one use [`IoBuf`], [`IoBufMut`] or
    /// [`SetBufInit`]
    #[allow(clippy::mut_from_ref)]
    unsafe fn as_slice_mut(&self) -> &mut [u8];

    unsafe fn initiated_len(&self) -> usize;

    unsafe fn set_initiated_len(&self, new_len: usize);
}

impl<T: IoBuf + Send + Sync> AsIoBuf for SharedBuf<T> {
    /// # Safety:
    ///
    /// user must make sure when use [`IoBuf`], no other one use [`IoBufMut`] or [`SetBufInit`]
    unsafe fn as_slice(&self) -> &[u8] {
        (*self.inner.get()).as_slice()
    }
}

impl<T: IoBufMut + Send + Sync> AsIoBufMut for SharedBuf<T> {
    unsafe fn as_slice_mut(&self) -> &mut [u8] {
        let slice = (*self.inner.get()).as_mut_slice();
        let len = slice.len();
        let ptr = slice.as_mut_ptr();

        slice::from_raw_parts_mut(ptr.cast(), len)
    }

    unsafe fn initiated_len(&self) -> usize {
        (*self.inner.get()).buf_len()
    }

    unsafe fn set_initiated_len(&self, new_len: usize) {
        (*self.inner.get()).set_buf_init(new_len)
    }
}
