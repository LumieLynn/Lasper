//! Finite process startup and FD responses on the blocking pool.

use sendfd::SendWithFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Only startup and sendmsg run here. The session's lifetime is owned by an
/// asynchronous process or machine monitor after the descriptors are sent.
pub(super) async fn respond<F>(stream: UnixStream, operation: F)
where
    F: FnOnce(&UnixStream) -> anyhow::Result<()> + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || {
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        if let Err(error) = operation(&stream) {
            let message = format!("{error:#}");
            log::warn!("Daemon session startup failed: {message}");
            let _ = stream.send_with_fd(message.as_bytes(), &[]);
        }
        std::io::Result::Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::error!("Daemon session FD worker failed: {error}"),
        Err(error) => log::error!("Daemon session FD worker panicked: {error}"),
    }
}
