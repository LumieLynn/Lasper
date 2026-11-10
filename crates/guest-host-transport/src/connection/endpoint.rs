use lasper_rootfs_path::RootfsAccess;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

pub(super) struct BrokerEndpoint {
    socket: File,
}

impl BrokerEndpoint {
    pub(super) fn open(root: &RootfsAccess, path: &str) -> io::Result<Option<Self>> {
        let Some(socket) = root.entry(path)? else {
            return Ok(None);
        };
        if !socket.metadata()?.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "guest broker endpoint is not a filesystem socket",
            ));
        }
        Ok(Some(Self { socket }))
    }

    pub(super) fn address(&self) -> PathBuf {
        // The proc link names the pinned inode, even if the guest replaces the
        // original socket path between inspection and connect.
        PathBuf::from(format!("/proc/self/fd/{}", self.socket.as_raw_fd()))
    }
}
