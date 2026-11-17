use super::*;
use std::future::Future;
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::FromRawFd,
};

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    // SAFETY: pipe2 initializes both slots on success.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: Each returned descriptor is transferred to exactly one owner.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[tokio::test]
async fn an_empty_read_completes_without_waiting_for_pipe_readiness() {
    let (reader, _writer) = pipe();
    let mut reader = PipeReader(Fd::new(reader, ReadEio::Error).unwrap());
    let mut read = Box::pin(tokio::io::AsyncReadExt::read(&mut reader, &mut []));
    assert!(matches!(
        std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await,
        Poll::Ready(Ok(0))
    ));
}

#[tokio::test]
async fn pipe_handoff_retains_nonblocking_mode_and_unread_output() {
    let (stdout, stdout_writer) = pipe();
    let (stderr, stderr_writer) = pipe();
    let SessionIo::Pipes { stdout, stderr } =
        SessionIo::from_ready(crate::PreparedIo::Pipes { stdout, stderr }).unwrap()
    else {
        unreachable!()
    };
    File::from(stdout_writer).write_all(b"stdout").unwrap();
    File::from(stderr_writer).write_all(b"stderr").unwrap();

    for (reader, expected) in [(stdout, b"stdout"), (stderr, b"stderr")] {
        let descriptor = reader.into_owned_fd();
        // SAFETY: fcntl only reads flags from this owned descriptor.
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        let mut output = [0; 6];
        File::from(descriptor).read_exact(&mut output).unwrap();
        assert_eq!(&output, expected);
    }
}
