use super::channel::invalid_data;
use super::IdentityPeerCredentials;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const MAX_STATUS_BYTES: u64 = 64 * 1024;

/// Process handles acquired from the worker pidfd supplied in PrepareReady.
/// The pidfd is checked for exit around procfs access: a proc directory opened
/// after PID reuse cannot become evidence for the assigned worker.
pub struct WorkerProcess {
    pidfd: OwnedFd,
    pid: i32,
    directory: File,
    pid_namespace: File,
    mount_namespace: File,
    user_namespace: File,
    root: File,
}

impl WorkerProcess {
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    pub const fn pid(&self) -> i32 {
        self.pid
    }

    pub fn directory(&self) -> BorrowedFd<'_> {
        self.directory.as_fd()
    }

    pub fn pid_namespace(&self) -> BorrowedFd<'_> {
        self.pid_namespace.as_fd()
    }

    pub fn mount_namespace(&self) -> BorrowedFd<'_> {
        self.mount_namespace.as_fd()
    }

    pub fn user_namespace(&self) -> BorrowedFd<'_> {
        self.user_namespace.as_fd()
    }

    pub fn root(&self) -> BorrowedFd<'_> {
        self.root.as_fd()
    }

    pub(crate) fn open(pidfd: OwnedFd) -> io::Result<Self> {
        let pid = pidfd_pid(pidfd.as_fd())?;
        check_pidfd(pidfd.as_fd())?;
        let directory = File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(format!("/proc/{pid}"))?;
        let process = Self {
            pid_namespace: open_at(&directory, c"ns/pid", libc::O_RDONLY)?,
            mount_namespace: open_at(&directory, c"ns/mnt", libc::O_RDONLY)?,
            user_namespace: open_at(&directory, c"ns/user", libc::O_RDONLY)?,
            root: open_at(&directory, c"root", libc::O_PATH | libc::O_DIRECTORY)?,
            directory,
            pidfd,
            pid,
        };
        process.recheck()?;
        Ok(process)
    }

    /// Checks process lifetime, namespaces and root. Machine membership and
    /// the requested guest account must still be verified by the caller.
    pub fn recheck(&self) -> io::Result<()> {
        check_pidfd(self.pidfd())?;
        for (handle, path) in [
            (&self.pid_namespace, c"ns/pid"),
            (&self.mount_namespace, c"ns/mnt"),
            (&self.user_namespace, c"ns/user"),
            (&self.root, c"root"),
        ] {
            let observed = open_at(&self.directory, path, libc::O_PATH)?;
            let expected = handle.metadata()?;
            let actual = observed.metadata()?;
            if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
                return Err(invalid_data(match path.to_bytes() {
                    b"ns/pid" => "identity worker changed PID namespace",
                    b"ns/mnt" => "identity worker changed mount namespace",
                    b"ns/user" => "identity worker changed user namespace",
                    _ => "identity worker changed root filesystem",
                }));
            }
        }
        check_pidfd(self.pidfd())
    }

    pub(super) fn verify_sender(&self, peer: &IdentityPeerCredentials) -> io::Result<()> {
        self.recheck()?;
        if peer.pid() != self.pid {
            return Err(invalid_data("identity reply came from a different worker"));
        }
        if let Some(sender) = peer.pidfd() {
            check_pidfd(sender)?;
            if pidfd_pid(sender)? != self.pid {
                return Err(invalid_data(
                    "identity packet pidfd does not match its sender",
                ));
            }
        }
        let mut status = Vec::new();
        open_at(&self.directory, c"status", libc::O_RDONLY)?
            .take(MAX_STATUS_BYTES + 1)
            .read_to_end(&mut status)?;
        if status.len() as u64 > MAX_STATUS_BYTES {
            return Err(invalid_data(
                "worker process status exceeded its size limit",
            ));
        }
        check_status(&status, self.pid, peer.uid(), peer.gid())?;
        self.recheck()
    }
}

fn open_at(directory: &File, path: &CStr, flags: i32) -> io::Result<File> {
    // SAFETY: The directory stays open. Each path is a fixed relative procfs
    // entry, and the returned descriptor is taken into one File owner.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            path.as_ptr(),
            flags | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor; File takes its sole ownership.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn pidfd_pid(pidfd: BorrowedFd<'_>) -> io::Result<i32> {
    // fdinfo translates the pidfd into the PID namespace of the host procfs,
    // unlike the guardian's PID number in the guest. Pid is absent on other
    // descriptor types and becomes -1 after the assigned process is reaped.
    let mut info = String::new();
    File::open(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?
        .take(4097)
        .read_to_string(&mut info)?;
    if info.len() > 4096 {
        return Err(invalid_data(
            "worker pidfd information exceeded its size limit",
        ));
    }
    let pid = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .ok_or_else(|| invalid_data("PrepareReady worker descriptor is not a pidfd"))?
        .trim()
        .parse::<i32>()
        .map_err(|_| invalid_data("worker pidfd contains an invalid PID"))?;
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "assigned worker has exited or is outside the host PID namespace",
        ));
    }
    Ok(pid)
}

fn check_pidfd(pidfd: BorrowedFd<'_>) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: poll borrows the live pidfd for this nonblocking check.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return if result == 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "identity worker has exited",
            ))
        };
    }
}

fn check_status(
    bytes: &[u8],
    expected_pid: i32,
    expected_uid: u32,
    expected_gid: u32,
) -> io::Result<()> {
    let mut tgid = None;
    let mut state = None;
    let mut uid = None;
    let mut gid = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let key = &line[..colon];
        if !matches!(key, b"Tgid" | b"State" | b"Uid" | b"Gid") {
            continue;
        }
        let value = std::str::from_utf8(&line[colon + 1..])
            .map_err(|_| invalid_data("worker process status has invalid credential fields"))?;
        match key {
            b"Tgid" if tgid.is_none() => tgid = value.trim().parse::<i32>().ok(),
            b"State" if state.is_none() => state = value.trim().chars().next(),
            b"Uid" if uid.is_none() => uid = Some(same_ids(value, expected_uid)),
            b"Gid" if gid.is_none() => gid = Some(same_ids(value, expected_gid)),
            _ => {}
        }
    }
    if tgid != Some(expected_pid) || uid != Some(true) || gid != Some(true) {
        return Err(invalid_data(
            "worker process credentials do not match its identity packet",
        ));
    }
    match state {
        Some('Z' | 'X' | 'x') | None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "identity worker has exited",
        )),
        Some(_) => Ok(()),
    }
}

fn same_ids(text: &str, expected: u32) -> bool {
    let mut ids = text.split_ascii_whitespace();
    (0..4).all(|_| ids.next().and_then(|id| id.parse::<u32>().ok()) == Some(expected))
        && ids.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_accepts_non_utf8_names_but_requires_complete_final_credentials() {
        let mut status = b"Name:\t\xffworker\nState:\tS (sleeping)\nTgid:\t42\n".to_vec();
        status.extend_from_slice(b"Uid:\t1000 1000 1000 1000\nGid:\t2000 2000 2000 2000\n");
        check_status(&status, 42, 1000, 2000).unwrap();
        for field in [
            "Uid:\t1000 0 1000 1000",
            "Gid:\t2000 2000 2000",
            "Tgid:\t43",
            "State:\tZ (zombie)",
        ] {
            let text = "State:\tS (sleeping)\nTgid:\t42\nUid:\t1000 1000 1000 1000\nGid:\t2000 2000 2000 2000\n";
            let key = field.split(':').next().unwrap();
            let text = text
                .lines()
                .map(|line| {
                    if line.starts_with(&format!("{key}:")) {
                        field
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                check_status(text.as_bytes(), 42, 1000, 2000).is_err(),
                "{field}"
            );
        }
    }
}
