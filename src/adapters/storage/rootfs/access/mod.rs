//! Resolve guest paths relative to an opened root, including guest-absolute symlinks.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) struct RootfsAccess {
    root: File,
}

impl RootfsAccess {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Self::from_directory(root)
    }

    /// Reuse a pinned running guest root without resolving its path again.
    pub(crate) fn from_directory(root: File) -> io::Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rootfs is not a directory",
            ));
        }
        Ok(Self { root })
    }

    pub(crate) fn entry(&self, path: &str) -> io::Result<Option<File>> {
        let path = CString::new(path).map_err(io::Error::other)?;
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
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

    pub(crate) fn regular_file(&self, path: &str) -> io::Result<Option<File>> {
        let Some(entry) = self.entry(path)? else {
            return Ok(None);
        };
        if !entry.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "guest path is not a regular file",
            ));
        }
        // Reopen the pinned inode only after checking its type. This cannot follow
        // a replaced guest symlink or open a guest device/FIFO while inspecting it.
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", entry.as_raw_fd()))
            .map(Some)
    }
}

#[cfg(test)]
mod tests;
