use super::*;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time::{timeout, Duration};

struct Broken;

impl AsyncWrite for Broken {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn sink_failure_does_not_wait_for_an_idle_guest_stream() {
    let mut stdout = b"diagnostic".as_slice();
    let (mut stderr, writer) = tokio::io::duplex(64);
    let writer = std::cell::RefCell::new(Some(writer));
    let error = timeout(
        Duration::from_secs(1),
        pipes(
            &mut stdout,
            &mut stderr,
            &mut Broken,
            &mut tokio::io::sink(),
            || async {
                writer.borrow_mut().take();
            },
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn both_streams_are_flushed_before_completion() {
    let (mut stdout, mut stderr) = (b"stdout".as_slice(), b"stderr".as_slice());
    let mut out = tokio::io::BufWriter::new(Vec::new());
    let mut err = tokio::io::BufWriter::new(Vec::new());
    pipes(&mut stdout, &mut stderr, &mut out, &mut err, || async {})
        .await
        .unwrap();
    assert_eq!(out.get_ref(), b"stdout");
    assert_eq!(err.get_ref(), b"stderr");
}
