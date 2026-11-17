use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::process::ProcessHandle;

const MAX_CGROUP_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hierarchy {
    Unified,
    Systemd(u32),
}

/// The unit's ControlGroup, not the leader's possibly narrower init.scope.
/// Membership paths must be read in the same cgroup namespace as this root.
pub(super) struct MachineCgroup {
    hierarchy: Hierarchy,
    root: PathBuf,
}

impl MachineCgroup {
    pub(super) fn new(root: &Path, leader: &ProcessHandle) -> io::Result<Self> {
        let root = checked_path(root.as_os_str().as_bytes())?.to_owned();
        let membership = Membership::read(leader)?;
        let group = Self {
            hierarchy: membership.hierarchy,
            root,
        };
        group.verify_membership(&membership)?;
        Ok(group)
    }

    pub(super) fn verify(&self, process: &ProcessHandle) -> io::Result<()> {
        self.verify_membership(&Membership::read(process)?)
    }

    fn verify_membership(&self, membership: &Membership) -> io::Result<()> {
        if membership.hierarchy != self.hierarchy || !membership.path.starts_with(&self.root) {
            return Err(invalid_data(
                "process is outside the target machine cgroup subtree",
            ));
        }
        Ok(())
    }
}

struct Membership {
    hierarchy: Hierarchy,
    path: PathBuf,
}

impl Membership {
    fn read(process: &ProcessHandle) -> io::Result<Self> {
        Self::parse(&process.read(c"cgroup", MAX_CGROUP_BYTES)?)
    }

    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let mut unified = None;
        let mut systemd = None;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let mut fields = line.splitn(3, |byte| *byte == b':');
            let id = fields.next().unwrap();
            let controllers = fields
                .next()
                .ok_or_else(|| invalid_data("invalid cgroup record"))?;
            let path = fields
                .next()
                .ok_or_else(|| invalid_data("invalid cgroup record"))?;
            let id = std::str::from_utf8(id)
                .ok()
                .and_then(|id| id.parse::<u32>().ok())
                .ok_or_else(|| invalid_data("invalid cgroup hierarchy ID"))?;
            let destination = if id == 0 && controllers.is_empty() {
                Some((&mut unified, Hierarchy::Unified))
            } else if id != 0
                && controllers
                    .split(|byte| *byte == b',')
                    .any(|name| name == b"name=systemd")
            {
                Some((&mut systemd, Hierarchy::Systemd(id)))
            } else {
                None
            };
            if let Some((slot, hierarchy)) = destination {
                if slot.is_some() {
                    return Err(invalid_data("duplicate systemd cgroup hierarchy"));
                }
                *slot = Some(Self {
                    hierarchy,
                    path: checked_path(path)?.to_owned(),
                });
            }
        }
        // Hybrid hosts may expose both. Their system manager owns the named
        // v1 hierarchy; the unified row is not its unit membership.
        systemd
            .or(unified)
            .ok_or_else(|| invalid_data("process has no systemd cgroup hierarchy"))
    }
}

fn checked_path(bytes: &[u8]) -> io::Result<&Path> {
    if !bytes.starts_with(b"/")
        || bytes.contains(&0)
        || bytes.contains(&b'\n')
        || bytes
            .split(|byte| *byte == b'/')
            .any(|part| matches!(part, b"." | b".."))
    {
        // ../ means the process lies outside the reader's cgroup namespace.
        // Normalizing it would turn missing visibility into false membership.
        return Err(invalid_data(
            "cgroup path is not visible as an absolute subtree",
        ));
    }
    Ok(Path::new(OsStr::from_bytes(bytes)))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "../../tests/target/cgroup.rs"]
mod tests;
