//! Resolve guest paths relative to an opened root, including guest-absolute symlinks.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("Lasper rootfs path resolution currently supports Linux only");

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub struct RootfsAccess {
    root: File,
}

impl RootfsAccess {
    pub fn open(path: &Path) -> io::Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Self::from_directory(root)
    }

    /// Reuse a pinned running guest root without resolving its path again.
    pub fn from_directory(root: File) -> io::Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rootfs is not a directory",
            ));
        }
        Ok(Self { root })
    }

    /// Pins an entry for metadata inspection; the O_PATH handle cannot read its contents.
    pub fn entry(&self, path: &str) -> io::Result<Option<File>> {
        let path = CString::new(path).map_err(io::Error::other)?;
        // SAFETY: open_how is an integer-only kernel argument structure.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
        // SAFETY: path and how remain valid for the syscall; root owns the directory FD.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root.as_raw_fd(),
                path.as_ptr(),
                &how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            // SAFETY: openat2 returned a new descriptor with no other owner.
            return Ok(Some(unsafe { File::from_raw_fd(fd as libc::c_int) }));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => Ok(None),
            Some(libc::ENOSYS) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "rootfs inspection requires Linux openat2 support",
            )),
            _ => Err(error),
        }
    }
}
