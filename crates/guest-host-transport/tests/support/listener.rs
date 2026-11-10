use std::io;
use std::mem::{offset_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

/// Keep bind and listen separate so tests can hold the startup window open.
pub struct BoundSocket(OwnedFd);

impl BoundSocket {
    pub fn bind(path: &Path) -> Self {
        // SAFETY: socket creates a new descriptor; ownership is adopted below.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        // SAFETY: fd was just created and has no other owner.
        let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: sockaddr_un contains only integers and a byte array.
        let mut address: libc::sockaddr_un = unsafe { zeroed() };
        let bytes = path.as_os_str().as_bytes();
        assert!(!bytes.contains(&0) && bytes.len() < address.sun_path.len());
        address.sun_family = libc::AF_UNIX as _;
        for (output, byte) in address.sun_path.iter_mut().zip(bytes) {
            *output = *byte as _;
        }
        let length = offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
        // SAFETY: length includes the zeroed pathname terminator; address and
        // the owned socket remain valid throughout bind.
        assert_eq!(
            unsafe {
                libc::bind(
                    socket.0.as_raw_fd(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length as _,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        socket
    }

    pub fn listen(self) -> UnixListener {
        // SAFETY: this is the bound stream socket owned by the fixture.
        assert_eq!(
            unsafe { libc::listen(self.0.as_raw_fd(), 8) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        UnixListener::from(self.0)
    }
}
