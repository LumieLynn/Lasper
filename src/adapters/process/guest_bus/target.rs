//! Pin a process generation and the kernel objects needed by the connector.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

pub(crate) struct GuestBusTarget {
    process: File,
    start_time: NonZeroU64,
    pid: File,
    mount: File,
    user: File,
    root: File,
}

impl GuestBusTarget {
    pub(crate) fn open(leader: u32) -> io::Result<Self> {
        if leader <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid guest leader PID",
            ));
        }
        let process = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(format!("/proc/{leader}"))?;
        let proc = pinned_process(&process);
        let target = Self {
            start_time: crate::adapters::process_identity::process_start_time(&proc)?,
            pid: File::open(proc.join("ns/pid"))?,
            mount: File::open(proc.join("ns/mnt"))?,
            user: File::open(proc.join("ns/user"))?,
            root: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(proc.join("root"))?,
            process,
        };
        target.verify()?;
        if same_object(
            &target.pid.metadata()?,
            &std::fs::metadata("/proc/self/ns/pid")?,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "machine leader shares the host PID namespace",
            ));
        }
        Ok(target)
    }

    pub(crate) fn verify(&self) -> io::Result<()> {
        let proc = pinned_process(&self.process);
        if crate::adapters::process_identity::process_start_time(&proc)? != self.start_time {
            return Err(changed("guest leader process generation changed"));
        }
        for (file, path, description) in [
            (&self.pid, "ns/pid", "guest PID namespace changed"),
            (&self.mount, "ns/mnt", "guest mount namespace changed"),
            (&self.user, "ns/user", "guest user namespace changed"),
            (&self.root, "root", "guest root filesystem changed"),
        ] {
            if !same_object(&file.metadata()?, &std::fs::metadata(proc.join(path))?) {
                return Err(changed(description));
            }
        }
        Ok(())
    }

    /// Only a wait hint: executable identity, not comm or the presence of an
    /// init binary, determines whether this leader is the guest's systemd.
    pub(crate) fn systemd_init(&self) -> io::Result<bool> {
        let executable = std::fs::metadata(pinned_process(&self.process).join("exe"))?;
        let root = crate::adapters::storage::rootfs::access::RootfsAccess::from_directory(
            self.root.try_clone()?,
        )?;
        for path in ["/usr/lib/systemd/systemd", "/lib/systemd/systemd"] {
            if let Some(candidate) = root.entry(path)? {
                if same_object(&executable, &candidate.metadata()?) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(super) fn raw_fds(&self) -> [RawFd; 4] {
        [
            self.pid.as_raw_fd(),
            self.mount.as_raw_fd(),
            self.user.as_raw_fd(),
            self.root.as_raw_fd(),
        ]
    }
}

fn pinned_process(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn same_object(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message)
}
