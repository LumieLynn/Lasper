use super::channel::invalid_data;
use super::{IdMap, IdentityPeerCredentials};
use crate::process::{check_pidfd, open_at, pidfd_pid, ProcessHandle};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

const MAX_STATUS_BYTES: u64 = 64 * 1024;

/// Process handles acquired from the worker pidfd supplied in PrepareReady.
/// The assigned process stays fixed across identity observations.
pub struct WorkerProcess {
    process: ProcessHandle,
}

impl WorkerProcess {
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.process.pidfd()
    }

    pub const fn pid(&self) -> i32 {
        self.process.pid()
    }

    pub fn directory(&self) -> BorrowedFd<'_> {
        self.process.directory()
    }

    pub fn pid_namespace(&self) -> BorrowedFd<'_> {
        self.process.pid_namespace()
    }

    pub fn mount_namespace(&self) -> BorrowedFd<'_> {
        self.process.mount_namespace()
    }

    pub fn user_namespace(&self) -> BorrowedFd<'_> {
        self.process.user_namespace().as_fd()
    }

    pub fn root(&self) -> BorrowedFd<'_> {
        self.process.root()
    }

    pub(crate) fn open(pidfd: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            process: ProcessHandle::open(pidfd)?,
        })
    }

    pub(crate) fn handle(&self) -> &ProcessHandle {
        &self.process
    }

    /// Checks process lifetime, namespaces and root. Machine membership and
    /// the requested guest account must still be verified by the caller.
    pub fn recheck(&self) -> io::Result<()> {
        self.process.recheck()
    }

    pub(super) fn verify_identity(
        &self,
        peer: &IdentityPeerCredentials,
        guest_uid: u32,
        guest_gid: u32,
    ) -> io::Result<()> {
        self.recheck()?;
        if peer.pid() != self.pid() {
            return Err(invalid_data("identity reply came from a different worker"));
        }
        if let Some(sender) = peer.pidfd() {
            check_pidfd(sender)?;
            if pidfd_pid(sender)? != self.pid() {
                return Err(invalid_data(
                    "identity packet pidfd does not match its sender",
                ));
            }
        }
        let status = self.process.read(c"status", MAX_STATUS_BYTES)?;
        check_status(&status, self.pid(), peer.uid(), peer.gid())?;
        let reader_namespace = File::open("/proc/thread-self/ns/user")?.metadata()?;
        let worker_namespace = self.process.user_namespace().metadata()?;
        let same_namespace = reader_namespace.dev() == worker_namespace.dev()
            && reader_namespace.ino() == worker_namespace.ino();
        // When the reader shares the worker's user namespace, uid_map's
        // second column points to the parent, not back to the reader.
        // SCM_CREDENTIALS already uses the reader's namespace in either case.
        for (path, guest_id, host_id, description) in [
            (c"uid_map", guest_uid, peer.uid(), "guest UID"),
            (c"gid_map", guest_gid, peer.gid(), "guest GID"),
        ] {
            let mapping = if same_namespace {
                None
            } else {
                Some(IdMap::read(open_at(
                    self.directory(),
                    path,
                    libc::O_RDONLY,
                )?)?)
            };
            if mapped_id(guest_id, mapping.as_ref()) != Some(host_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{description} does not map to the identity packet's kernel credentials"
                    ),
                ));
            }
        }
        self.recheck()
    }
}

fn mapped_id(id: u32, mapping: Option<&IdMap>) -> Option<u32> {
    match mapping {
        Some(mapping) => mapping.map(id),
        None => (id != u32::MAX).then_some(id),
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
    fn shared_user_namespace_keeps_ids_in_the_readers_namespace() {
        let parent_map = IdMap::parse("0 100000 65536\n").unwrap();
        assert_eq!(mapped_id(1000, None), Some(1000));
        assert_eq!(mapped_id(1000, Some(&parent_map)), Some(101000));
        assert_eq!(mapped_id(u32::MAX, None), None);
        assert_eq!(mapped_id(65536, Some(&parent_map)), None);
    }

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
