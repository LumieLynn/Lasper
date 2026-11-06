use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

// Both the machine leader and assigned worker need the same lifetime checks.
// An open proc directory alone does not prove it was opened before PID reuse.
pub(crate) struct ProcessHandle {
    pidfd: OwnedFd,
    pid: i32,
    directory: File,
    pid_namespace: File,
    mount_namespace: File,
    user_namespace: File,
    root: File,
}

impl ProcessHandle {
    pub(crate) fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    pub(crate) const fn pid(&self) -> i32 {
        self.pid
    }

    pub(crate) fn directory(&self) -> BorrowedFd<'_> {
        self.directory.as_fd()
    }

    pub(crate) fn pid_namespace(&self) -> BorrowedFd<'_> {
        self.pid_namespace.as_fd()
    }

    pub(crate) fn mount_namespace(&self) -> BorrowedFd<'_> {
        self.mount_namespace.as_fd()
    }

    pub(crate) fn user_namespace(&self) -> &File {
        &self.user_namespace
    }

    pub(crate) fn root(&self) -> BorrowedFd<'_> {
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
            pid_namespace: open_at(directory.as_fd(), c"ns/pid", libc::O_RDONLY)?,
            mount_namespace: open_at(directory.as_fd(), c"ns/mnt", libc::O_RDONLY)?,
            user_namespace: open_at(directory.as_fd(), c"ns/user", libc::O_RDONLY)?,
            root: open_at(directory.as_fd(), c"root", libc::O_PATH | libc::O_DIRECTORY)?,
            directory,
            pidfd,
            pid,
        };
        process.recheck()?;
        Ok(process)
    }

    pub(crate) fn recheck(&self) -> io::Result<()> {
        check_pidfd(self.pidfd())?;
        for (handle, path) in [
            (&self.pid_namespace, c"ns/pid"),
            (&self.mount_namespace, c"ns/mnt"),
            (&self.user_namespace, c"ns/user"),
            (&self.root, c"root"),
        ] {
            let observed = open_at(self.directory.as_fd(), path, libc::O_PATH)?;
            let expected = handle.metadata()?;
            let actual = observed.metadata()?;
            if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
                return Err(invalid_data(match path.to_bytes() {
                    b"ns/pid" => "process changed PID namespace",
                    b"ns/mnt" => "process changed mount namespace",
                    b"ns/user" => "process changed user namespace",
                    _ => "process changed root filesystem",
                }));
            }
        }
        check_pidfd(self.pidfd())
    }

    pub(crate) fn read(&self, path: &CStr, limit: u64) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        open_at(self.directory.as_fd(), path, libc::O_RDONLY)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("process {} exceeded its size limit", path.to_string_lossy()),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn start_time(&self) -> io::Result<NonZeroU64> {
        self.recheck()?;
        let value = parse_start_time(&self.read(c"stat", 8 * 1024)?)?;
        self.recheck()?;
        Ok(value)
    }

    pub(crate) fn verify_location(&self, expected: &Self) -> io::Result<()> {
        for (actual, expected, description) in [
            (
                &self.pid_namespace,
                &expected.pid_namespace,
                "PID namespace",
            ),
            (
                &self.mount_namespace,
                &expected.mount_namespace,
                "mount namespace",
            ),
            (
                &self.user_namespace,
                &expected.user_namespace,
                "user namespace",
            ),
            (&self.root, &expected.root, "root filesystem"),
        ] {
            let actual = actual.metadata()?;
            let expected = expected.metadata()?;
            if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("process does not share the target {description}"),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn open_at(directory: BorrowedFd<'_>, path: &CStr, flags: i32) -> io::Result<File> {
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

pub(crate) fn pidfd_pid(pidfd: BorrowedFd<'_>) -> io::Result<i32> {
    // fdinfo translates the pidfd into the PID namespace of the host procfs,
    // unlike the guardian's PID number in the guest. Pid is absent on other
    // descriptor types and becomes -1 after the assigned process is reaped.
    let mut info = String::new();
    File::open(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?
        .take(4097)
        .read_to_string(&mut info)?;
    if info.len() > 4096 {
        return Err(invalid_data(
            "process pidfd information exceeded its size limit",
        ));
    }
    let pid = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .ok_or_else(|| invalid_data("process descriptor is not a pidfd"))?
        .trim()
        .parse::<i32>()
        .map_err(|_| invalid_data("process pidfd contains an invalid PID"))?;
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process has exited or is outside the host PID namespace",
        ));
    }
    Ok(pid)
}

pub(crate) fn check_pidfd(pidfd: BorrowedFd<'_>) -> io::Result<()> {
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
                "process has exited",
            ))
        };
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_start_time(stat: &[u8]) -> io::Result<NonZeroU64> {
    // comm can contain ')' and non-UTF-8 bytes. Numeric fields begin after
    // its last ')'; starttime is field 22, with state counting as field 3.
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| invalid_data("process stat has no command terminator"))?;
    let fields = std::str::from_utf8(&stat[end + 1..])
        .map_err(|_| invalid_data("process stat has invalid numeric fields"))?;
    let value = fields
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| invalid_data("process stat has no start time"))?
        .parse::<u64>()
        .map_err(|_| invalid_data("process stat has an invalid start time"))?;
    NonZeroU64::new(value).ok_or_else(|| invalid_data("process start time is zero"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(start: &str) -> Vec<u8> {
        let fields = std::iter::repeat_n("0", 18).collect::<Vec<_>>().join(" ");
        format!("42 (a process ) name) S {fields} {start} 0\n").into_bytes()
    }

    #[test]
    fn start_time_ignores_parentheses_spaces_and_non_utf8_in_comm() {
        let mut bytes = stat("123");
        assert_eq!(parse_start_time(&bytes).unwrap().get(), 123);
        bytes[5] = 0xff;
        assert_eq!(parse_start_time(&bytes).unwrap().get(), 123);
    }

    #[test]
    fn missing_zero_or_invalid_start_time_is_not_instance_evidence() {
        for bytes in [
            b"".to_vec(),
            b"42 (process) S".to_vec(),
            stat("0"),
            stat("-1"),
            stat("18446744073709551616"),
            stat("invalid"),
        ] {
            assert_eq!(
                parse_start_time(&bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
