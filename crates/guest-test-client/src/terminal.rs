use anyhow::{Context, Result};
use lasper_guest_protocol::TerminalSize;
use std::{
    fs::{File, OpenOptions},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::{
    io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf},
    signal::unix::{signal, SignalKind},
};

pub struct Terminal {
    file: File,
    original: libc::termios,
}

impl Terminal {
    pub fn open() -> Result<Self> {
        // A separate open file description keeps O_NONBLOCK changes off stdin.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(crate::paths::CONTROLLING_TERMINAL)
            .context("interactive mode requires a controlling host terminal")?;
        let mut original = MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(file.as_raw_fd(), original.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            file,
            original: unsafe { original.assume_init() },
        })
    }
    pub fn size(&self) -> Result<TerminalSize> {
        size(self.file.as_raw_fd())
    }
    pub async fn relay(&self, master: OwnedFd) -> Result<()> {
        let mut raw = self.original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // The restore guard also runs when the relay future is cancelled.
        let _restore = Restore {
            fd: self.file.as_raw_fd(),
            original: self.original,
        };
        let host = Fd::new(self.file.try_clone()?.into(), ReadEio::Error)?;
        let guest = Fd::new(master, ReadEio::PtyEof)?;
        let master_fd = guest.io.get_ref().as_raw_fd();
        let mut resize = signal(SignalKind::window_change())?;
        // A fresh size read after subscription covers resizes during Prepare/Start.
        // Later SIGWINCH notifications cover subsequent size changes.
        set_size(master_fd, self.size()?)?;
        let (mut host_read, mut host_write) = tokio::io::split(host);
        let (mut guest_read, mut guest_write) = tokio::io::split(guest);
        // Recreating copy futures after a resize would discard buffered bytes.
        // Guest output must still drain after the guest stops accepting input.
        let input = tokio::io::copy(&mut host_read, &mut guest_write);
        let output = tokio::io::copy(&mut guest_read, &mut host_write);
        tokio::pin!(input, output);
        let mut input_done = false;
        loop {
            tokio::select! {
                result = &mut output => { result?; return Ok(()); }
                result = &mut input, if !input_done => {
                    input_done = true;
                    match result {
                        Err(error) if matches!(error.raw_os_error(), Some(libc::EIO | libc::EPIPE)) => {},
                        Err(error) => return Err(error.into()),
                        Ok(_) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "host terminal closed").into()),
                    }
                }
                _ = resize.recv() => {
                    set_size(master_fd, self.size()?)?;
                }
            }
        }
    }
}

struct Restore {
    fd: i32,
    original: libc::termios,
}
impl Drop for Restore {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}
fn size(fd: i32) -> Result<TerminalSize> {
    let mut window = MaybeUninit::<libc::winsize>::uninit();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, window.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let window = unsafe { window.assume_init() };
    TerminalSize::new(window.ws_col.max(1), window.ws_row.max(1))
        .map_err(|e| anyhow::anyhow!("invalid terminal size: {e:?}"))
}

fn set_size(fd: i32, size: TerminalSize) -> Result<()> {
    let window = libc::winsize {
        ws_row: size.rows(),
        ws_col: size.cols(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &window) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ReadEio {
    Error,
    // A Linux PTY master reports EIO once the last slave has closed.
    PtyEof,
}

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
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::{timeout, Duration},
    };

    // SIGWINCH is process-wide. Exercise these scenarios in one runtime, as
    // the real client does, without unrelated terminal signal subscribers.
    #[tokio::test]
    async fn terminal_io() {
        relay_preserves_binary_data_resizes_and_restores_on_cancellation().await;
        relay_drains_final_output_and_restores_on_guest_exit().await;
        read_eio_policy_distinguishes_pty_eof_from_errors().await;
    }

    fn pty() -> (OwnedFd, OwnedFd) {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }
    fn attributes(fd: i32) -> libc::termios {
        let mut value = MaybeUninit::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
        unsafe { value.assume_init() }
    }

    async fn relay_preserves_binary_data_resizes_and_restores_on_cancellation() {
        let mut phase = "setup";
        let result = timeout(Duration::from_secs(5), async {
            let (host_master, host_slave) = pty();
            let original = attributes(host_slave.as_raw_fd());
            let check = host_slave.try_clone().unwrap();
            let terminal = Terminal {
                file: File::from(host_slave),
                original,
            };
            let (guest_master, guest_slave) = pty();
            let mut raw = attributes(guest_slave.as_raw_fd());
            unsafe { libc::cfmakeraw(&mut raw) };
            assert_eq!(
                unsafe { libc::tcsetattr(guest_slave.as_raw_fd(), libc::TCSANOW, &raw) },
                0
            );
            let task = tokio::spawn(async move { terminal.relay(guest_master).await });
            let mut host = Fd::new(host_master, ReadEio::PtyEof).unwrap();
            let mut guest = Fd::new(guest_slave, ReadEio::Error).unwrap();
            phase = "enter raw mode";
            // Wait for raw mode before sending bytes which canonical mode
            // would interpret (including Ctrl-C and Ctrl-D).
            while attributes(check.as_raw_fd()).c_lflag & libc::ICANON != 0 {
                tokio::task::yield_now().await;
            }
            let payload: Vec<u8> = (0..65_536).map(|n| (n % 256) as u8).collect();
            let mut received = vec![0; payload.len()];
            phase = "forward host input";
            let (write, read) =
                tokio::join!(host.write_all(&payload), guest.read_exact(&mut received));
            write.unwrap();
            read.unwrap();
            assert_eq!(received, payload);

            phase = "propagate resize";
            let window = libc::winsize {
                ws_row: 41,
                ws_col: 113,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(host.io.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &window) },
                0
            );
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGWINCH) }, 0);
            while size(guest.io.get_ref().as_raw_fd()).unwrap()
                != TerminalSize::new(113, 41).unwrap()
            {
                tokio::task::yield_now().await;
            }
            // Exercise the opposite direction after a resize; the persistent
            // relay future must retain its original buffered-copy state.
            phase = "forward guest output";
            let (write, read) =
                tokio::join!(guest.write_all(&payload), host.read_exact(&mut received));
            write.unwrap();
            read.unwrap();
            assert_eq!(received, payload);
            phase = "restore terminal";
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let restored = attributes(check.as_raw_fd());
            assert_eq!(restored.c_lflag, original.c_lflag);
            assert_eq!(restored.c_iflag, original.c_iflag);
            assert_eq!(restored.c_oflag, original.c_oflag);
            assert_eq!(restored.c_cc, original.c_cc);
        })
        .await;
        assert!(result.is_ok(), "terminal test timed out: {phase}");
    }

    async fn relay_drains_final_output_and_restores_on_guest_exit() {
        timeout(Duration::from_secs(3), async {
            use std::io::Write;
            let (host_master, host_slave) = pty();
            let original = attributes(host_slave.as_raw_fd());
            let check = host_slave.try_clone().unwrap();
            let terminal = Terminal {
                file: File::from(host_slave),
                original,
            };
            let (guest_master, guest_slave) = pty();
            let mut guest = File::from(guest_slave);
            let task = tokio::spawn(async move { terminal.relay(guest_master).await });
            let mut host = Fd::new(host_master, ReadEio::PtyEof).unwrap();
            while attributes(check.as_raw_fd()).c_lflag & libc::ICANON != 0 {
                tokio::task::yield_now().await;
            }
            guest.write_all(b"final guest output").unwrap();
            drop(guest);
            let mut output = [0; 18];
            host.read_exact(&mut output).await.unwrap();
            assert_eq!(&output, b"final guest output");
            task.await.unwrap().unwrap();
            assert_eq!(attributes(check.as_raw_fd()).c_lflag, original.c_lflag);
        })
        .await
        .unwrap();
    }

    async fn read_eio_policy_distinguishes_pty_eof_from_errors() {
        let (master, slave) = pty();
        drop(slave);
        let mut master = Fd::new(master, ReadEio::PtyEof).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), master.read(&mut [0; 16]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let (master, slave) = pty();
        drop(slave);
        let mut terminal = Fd::new(master, ReadEio::Error).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), terminal.read(&mut [0; 16]))
                .await
                .unwrap()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EIO)
        );
    }
}
