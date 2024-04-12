use std::io::{Error, Read, Result, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::io;

#[repr(transparent)]
#[derive(Debug)]
pub struct TapFd(OwnedFd);

impl From<OwnedFd> for TapFd {
    fn from(value: OwnedFd) -> Self {
        Self(value)
    }
}

impl AsFd for TapFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for &TapFd {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        io::read(&self.0, buf).map_err(Error::from)
    }
}

impl Write for &TapFd {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        io::write(&self.0, buf).map_err(Error::from)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
