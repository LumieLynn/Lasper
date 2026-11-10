use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use tokio::net::UnixStream;

use crate::process::ProcessHandle;
use crate::MachineTarget;

pub(super) fn identify(stream: &UnixStream, target: &MachineTarget) -> io::Result<ProcessHandle> {
    let pidfd = peer_pidfd(stream)?;
    let peer = ProcessHandle::open(pidfd)?;
    // SAFETY: ucred has only integer fields; getsockopt writes within its size.
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() || credentials.pid != peer.pid() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker peer credentials do not match its pidfd",
        ));
    }
    target.verify_process(&peer)?;
    Ok(peer)
}

fn peer_pidfd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SO_PEERPIDFD pins the listener's process, including when a forked
    // guardian accepts the connection. Opening SO_PEERCRED.pid afterward
    // would instead risk adopting a reused PID.
    // SAFETY: fd and length are valid output storage for this socket option.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ENOPROTOOPT | libc::EOPNOTSUPP | libc::EINVAL) => io::Error::new(
                io::ErrorKind::Unsupported,
                "helper broker verification requires Linux SO_PEERPIDFD support",
            ),
            _ => error,
        });
    }
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker socket returned no peer pidfd",
        ));
    }
    // SAFETY: successful SO_PEERPIDFD installs a new descriptor for the caller.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
    if length as usize != size_of::<libc::c_int>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker socket returned an invalid peer pidfd length",
        ));
    }
    Ok(pidfd)
}
