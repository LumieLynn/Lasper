use super::*;
use std::os::fd::FromRawFd;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{timeout, Duration},
};

#[tokio::test]
async fn empty_io_does_not_wait_for_readiness() {
    use std::future::Future;
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut fd = Fd::new(socket.into(), ReadEio::Error).unwrap();
    {
        let mut empty = [];
        let mut read = std::pin::pin!(fd.read(&mut empty));
        assert!(matches!(
            std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await,
            Poll::Ready(Ok(0))
        ));
    }
    assert_eq!(fd.write(&[]).await.unwrap(), 0);
}

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
        let (write, read) = tokio::join!(host.write_all(&payload), guest.read_exact(&mut received));
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
        while size(guest.io.get_ref().as_raw_fd()).unwrap() != TerminalSize::new(113, 41).unwrap() {
            tokio::task::yield_now().await;
        }
        // Exercise the opposite direction after a resize; the persistent
        // relay future must retain its original buffered-copy state.
        phase = "forward guest output";
        let (write, read) = tokio::join!(guest.write_all(&payload), host.read_exact(&mut received));
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
