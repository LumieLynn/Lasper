//! Inspect guest entries and read regular files through a pinned root.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) struct RootfsAccess {
    root: lasper_rootfs_path::RootfsAccess,
}

impl RootfsAccess {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        lasper_rootfs_path::RootfsAccess::open(path).map(|root| Self { root })
    }

    /// Keep the root already pinned to a running machine instance.
    pub(crate) fn from_directory(root: File) -> io::Result<Self> {
        lasper_rootfs_path::RootfsAccess::from_directory(root).map(|root| Self { root })
    }

    /// Inspect an entry without opening guest devices or FIFOs for I/O.
    pub(crate) fn entry(&self, path: &str) -> io::Result<Option<File>> {
        self.root.entry(path)
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
        // Reopen the checked inode, not the guest path: it may have been replaced
        // with a device or FIFO since entry() pinned it.
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", entry.as_raw_fd()))
            .map(Some)
    }
}

#[cfg(test)]
mod tests;
