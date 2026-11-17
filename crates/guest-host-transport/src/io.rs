//! Ready data descriptors are independent of the control session.
//!
//! Closing the last PTY master can send SIGHUP to its foreground process group.
//! Closing a pipe reader can cause SIGPIPE in the command. Dropping these values
//! therefore changes the command's I/O; use [`crate::Session::close`] to observe cleanup.
use lasper_guest_protocol::TerminalSize;
use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

/// Data owners can be drained independently of Session operations. Dropping them
/// closes the local endpoints and can terminate a writer or PTY foreground group.
#[derive(Debug)]
pub enum SessionIo {
    Terminal(PtyMaster),
    Pipes {
        stdout: PipeReader,
        stderr: PipeReader,
    },
}

impl SessionIo {
    pub(crate) fn from_ready(io: crate::PreparedIo) -> io::Result<Self> {
        match io {
            crate::PreparedIo::Terminal(fd) => {
                Ok(Self::Terminal(PtyMaster(Fd::new(fd, ReadEio::PtyEof)?)))
            }
            crate::PreparedIo::Pipes { stdout, stderr } => Ok(Self::Pipes {
                stdout: PipeReader(Fd::new(stdout, ReadEio::Error)?),
                stderr: PipeReader(Fd::new(stderr, ReadEio::Error)?),
            }),
        }
    }
}

/// The caller must wait for Start acceptance before writing and stop on Closing.
/// This descriptor does not track session phase; early input stays in the PTY
/// buffer and can be consumed by the program after exec.
#[derive(Debug)]
pub struct PtyMaster(Fd);
#[derive(Debug)]
pub struct PipeReader(Fd);

impl PtyMaster {
    pub fn resize(&self, size: TerminalSize) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows(),
            ws_col: size.cols(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: The ioctl borrows a live winsize and the owned PTY descriptor.
        if unsafe { libc::ioctl(self.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn into_owned_fd(self) -> OwnedFd {
        self.0.io.into_inner()
    }
}
impl AsFd for PtyMaster {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.io.get_ref().as_fd()
    }
}
impl AsRawFd for PtyMaster {
    fn as_raw_fd(&self) -> i32 {
        self.as_fd().as_raw_fd()
    }
}
impl AsFd for PipeReader {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.io.get_ref().as_fd()
    }
}

impl PipeReader {
    /// Transfers the read end with its nonblocking flag unchanged.
    pub fn into_owned_fd(self) -> OwnedFd {
        self.0.io.into_inner()
    }
}
impl AsyncRead for PtyMaster {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}
impl AsyncRead for PipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}
impl AsyncWrite for PtyMaster {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

#[derive(Clone, Copy, Debug)]
enum ReadEio {
    Error,
    // A Linux PTY master reports EIO once the last slave has closed.
    PtyEof,
}

#[derive(Debug)]
struct Fd {
    io: AsyncFd<OwnedFd>,
    read_eio: ReadEio,
}
impl Fd {
    fn new(fd: OwnedFd, read_eio: ReadEio) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: AsyncFd owns the descriptor until it is dropped. No code
        // replaces or closes the descriptor through a borrowed raw fd.
        Ok(Self {
            io: unsafe { AsyncFd::register(fd) }.map_err(io::Error::from)?,
            read_eio,
        })
    }
}
impl AsyncRead for Fd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let mut ready = std::task::ready!(self.io.poll_read_ready(cx))?;
            let result = ready.try_io(|fd| {
                let bytes = buffer.initialize_unfilled();
                let count = unsafe {
                    libc::read(
                        fd.get_ref().as_raw_fd(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                };
                if count >= 0 {
                    return Ok(count as usize);
                }
                let error = io::Error::last_os_error();
                if matches!(self.read_eio, ReadEio::PtyEof)
                    && error.raw_os_error() == Some(libc::EIO)
                {
                    Ok(0)
                } else {
                    Err(error)
                }
            });
            match result {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => {
                    buffer.advance(result?);
                    return Poll::Ready(Ok(()));
                }
                Err(_) => continue,
            }
        }
    }
}
impl AsyncWrite for Fd {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            let mut ready = std::task::ready!(self.io.poll_write_ready(cx))?;
            match ready.try_io(|fd| {
                let count = unsafe {
                    libc::write(fd.get_ref().as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                };
                if count >= 0 {
                    Ok(count as usize)
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        // A PTY has no write-half close. Dropping its owner closes both directions.
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
#[path = "../tests/io/ownership.rs"]
mod tests;
