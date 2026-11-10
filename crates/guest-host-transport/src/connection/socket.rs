use std::ffi::CString;
use std::io;
use std::mem::{offset_of, size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tokio::net::UnixStream;

pub(super) fn connect(address: &Path) -> io::Result<UnixStream> {
    let (address, length) = socket_address(address)?;
    let socket = new_socket()?;
    // SAFETY: address is initialized, length includes its terminating NUL,
    // and socket remains owned until connect finishes.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result < 0 {
        // Linux connect(2) specifies EAGAIN rather than EINPROGRESS for a
        // nonblocking AF_UNIX connection that cannot complete immediately.
        // The caller retries; unexpected errors are returned without waiting.
        return Err(io::Error::last_os_error());
    }
    UnixStream::from_std(std::os::unix::net::UnixStream::from(socket))
}

fn new_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket creates one new descriptor, with nonblocking and CLOEXEC
    // set before it can become visible to any asynchronous operation.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful socket call returned an unowned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn socket_address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "broker address contains NUL"))?;
    // SAFETY: sockaddr_un contains only integer fields and a byte array.
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    let bytes = path.as_bytes_with_nul();
    if bytes.len() > address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker address is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (output, byte) in address.sun_path.iter_mut().zip(bytes) {
        *output = *byte as _;
    }
    let length = offset_of!(libc::sockaddr_un, sun_path) + bytes.len();
    debug_assert!(length <= size_of::<libc::sockaddr_un>());
    Ok((address, length as libc::socklen_t))
}
