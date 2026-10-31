//! Connect a trusted host process to a guest bus using pinned namespace FDs.
//! The connector never executes a guest binary and exits before zbus starts.

mod channel;
mod connector;
mod target;

pub(crate) use target::GuestBusTarget;

use super::ProcessGroupChild;
use sendfd::SendWithFd;
use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::process::Stdio;
use tokio::time::Instant;

const INTERNAL_ARGUMENT: &str = "--internal-guest-bus-connect";

#[derive(Debug, thiserror::Error)]
pub(crate) enum GuestBusError {
    #[error("prepare guest bus connection: {0}")]
    Preparation(#[from] io::Error),
    #[error("connect guest system bus: {0}")]
    Socket(io::Error),
    #[error("guest bus connector exceeded its deadline")]
    Deadline,
}

/// This entry must run before configuration, logging, or a Tokio runtime.
/// Its only input is an inherited socket; no public request supplies raw FDs.
pub(crate) fn dispatch_connector() -> Option<io::Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_ARGUMENT)) {
        return None;
    }
    if args.next().is_some() {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest bus connector accepts only its inherited control socket",
        )));
    }
    // Duplicate only a valid inherited descriptor; invoking the internal flag
    // with stdin closed must return an error rather than construct an invalid FD.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Some(Err(io::Error::last_os_error()));
    }
    let channel = unsafe { UnixDatagram::from_raw_fd(fd) };
    Some(connector::run(channel))
}

pub(crate) async fn connect(
    target: &GuestBusTarget,
    deadline: Instant,
) -> Result<tokio::net::UnixStream, GuestBusError> {
    if Instant::now() >= deadline {
        return Err(GuestBusError::Deadline);
    }
    target.verify()?;
    let (parent, child) = UnixDatagram::pair()?;
    parent.set_nonblocking(true)?;
    let channel = tokio::net::UnixDatagram::from_std(parent)?;
    // /proc/self/exe pins the running host image even when an installation is
    // replaced. No guest path or fixed installation prefix participates.
    let mut command = tokio::process::Command::new("/proc/self/exe");
    command
        .arg(INTERNAL_ARGUMENT)
        .stdin(Stdio::from(OwnedFd::from(child)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut process = ProcessGroupChild::spawn(command, "guest bus connector")?;
    let exchange = async {
        loop {
            channel.writable().await?;
            match channel.try_io(tokio::io::Interest::WRITABLE, || {
                channel.send_with_fd(b"connect", &target.raw_fds())
            }) {
                Ok(7) => break,
                Ok(_) => return Err(invalid_response("short namespace descriptor request").into()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let (socket_error, status, descriptors) = receive(&channel).await?;
        let exit = process.child_mut().wait().await?;
        process.completed();
        if !exit.success() {
            return Err(io::Error::other(format!("guest bus connector exited with {exit}")).into());
        }
        if status != 0 {
            let error = io::Error::from_raw_os_error(status);
            return Err(if socket_error {
                GuestBusError::Socket(error)
            } else {
                error.into()
            });
        }
        if descriptors.len() != 1 {
            return Err(invalid_response("connector did not return one bus socket").into());
        }
        let stream = std::os::unix::net::UnixStream::from(descriptors.into_iter().next().unwrap());
        stream.set_nonblocking(true)?;
        tokio::net::UnixStream::from_std(stream).map_err(Into::into)
    };
    tokio::time::timeout_at(deadline, exchange)
        .await
        .map_err(|_| GuestBusError::Deadline)?
}

async fn receive(channel: &tokio::net::UnixDatagram) -> io::Result<(bool, i32, Vec<OwnedFd>)> {
    loop {
        channel.readable().await?;
        let mut bytes = [0u8; 8];
        match channel.try_io(tokio::io::Interest::READABLE, || {
            channel::receive(channel.as_fd(), &mut bytes)
        }) {
            Ok((count, descriptors)) => {
                if count != bytes.len() {
                    return Err(invalid_response("invalid guest bus connector response"));
                }
                let socket_error = match u32::from_ne_bytes(bytes[..4].try_into().unwrap()) {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid_response("invalid connector error stage")),
                };
                let status = i32::from_ne_bytes(bytes[4..].try_into().unwrap());
                return Ok((socket_error, status, descriptors));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn invalid_response(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
