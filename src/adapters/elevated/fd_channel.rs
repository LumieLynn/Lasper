//! Authenticated FD requests and bounded SCM_RIGHTS response ownership.

use crate::domain::secret::SecretBytes;
use crate::ipc::protocol::{FdOperation, FdRequest};
use crate::ipc::transport::{authorize_root_server, get_peer_credentials};
use sendfd::RecvWithFd;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FD_CHANNEL_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct FdChannel {
    path: PathBuf,
    auth_token: Arc<str>,
}

impl FdChannel {
    pub(super) fn new(path: PathBuf, auth_token: Arc<str>) -> Self {
        Self { path, auth_token }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    // Authenticate asynchronously, then hand the blocking socket to the FD
    // receiver. Mixing sendfd's try_io with Tokio reads on one socket is unreliable.
    pub(super) async fn open(
        &self,
        operation: FdOperation,
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        use tokio::io::AsyncWriteExt;

        tokio::time::timeout(FD_CHANNEL_SETUP_TIMEOUT, async {
            let mut sock = tokio::net::UnixStream::connect(&self.path).await?;
            authorize_root_server(get_peer_credentials(&sock)?)?;
            let request = FdRequest {
                auth_token: self.auth_token.to_string(),
                operation,
            };
            let request_line = SecretBytes::new(
                serde_json::to_vec(&request)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            );
            sock.write_all(request_line.as_slice()).await?;
            sock.write_all(b"\n").await?;

            let std_sock = sock.into_std()?;
            std_sock.set_nonblocking(false)?;
            Ok(std_sock)
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out opening daemon FD channel after {}s",
                    FD_CHANNEL_SETUP_TIMEOUT.as_secs()
                ),
            )
        })?
    }
}

pub(crate) fn pipe_reader_owned(
    owned: OwnedFd,
) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    tokio::net::unix::pipe::Receiver::from_owned_fd(owned)
}

/// Receive one daemon FD response without exposing borrowed descriptor
/// numbers to the async caller. Both session handoffs and deployment streams
/// use this same `recvmsg` frame: a short status payload plus SCM_RIGHTS.
///
/// The blocking syscall runs on a worker thread, so the socket-level deadline
/// is essential: cancelling the JoinHandle alone cannot interrupt `recvmsg`.
pub(super) fn receive_fd_response(
    socket: &std::os::unix::net::UnixStream,
    operation: &str,
    timeout: std::time::Duration,
) -> std::io::Result<(String, Vec<OwnedFd>)> {
    let mut buffer = [0u8; 512];
    let mut fds = [-1 as RawFd; 2];
    let deadline = std::time::Instant::now() + timeout;
    let (read, fd_count) = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(fd_receive_timeout(operation, timeout));
        }
        socket.set_read_timeout(Some(remaining))?;
        match socket.recv_with_fd(&mut buffer, &mut fds) {
            Ok(received) => break received,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(map_fd_receive_error(error, operation, timeout)),
        }
    };

    // Take ownership before returning from the worker. If the caller is
    // cancelled after this point, Vec<OwnedFd> still closes every descriptor.
    let fds = fds[..fd_count]
        .iter()
        .copied()
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect::<Vec<_>>();
    let message = String::from_utf8(buffer[..read].to_vec())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok((message, fds))
}

fn map_fd_receive_error(
    error: std::io::Error,
    operation: &str,
    timeout: std::time::Duration,
) -> std::io::Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        fd_receive_timeout(operation, timeout)
    } else {
        error
    }
}

fn fd_receive_timeout(operation: &str, timeout: std::time::Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("timed out waiting for daemon {operation} FD response after {timeout:?}"),
    )
}
