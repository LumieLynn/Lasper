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
mod tests {
    use super::*;

    fn group(root: &str, hierarchy: Hierarchy) -> MachineCgroup {
        MachineCgroup {
            root: root.into(),
            hierarchy,
        }
    }

    #[test]
    fn leader_and_services_can_occupy_different_descendants_of_the_unit() {
        let group = group("/machine.slice/machine-test.scope", Hierarchy::Unified);
        for path in [
            "/machine.slice/machine-test.scope",
            "/machine.slice/machine-test.scope/init.scope",
            "/machine.slice/machine-test.scope/system.slice/lasper.service",
        ] {
            group
                .verify_membership(&Membership::parse(format!("0::{path}\n").as_bytes()).unwrap())
                .unwrap();
        }
        for path in [
            "/machine.slice/machine-test.scope-other",
            "/machine.slice/machine-other.scope/system.slice/lasper.service",
            "/machine.slice",
            "/",
        ] {
            assert!(
                group
                    .verify_membership(
                        &Membership::parse(format!("0::{path}\n").as_bytes()).unwrap()
                    )
                    .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn legacy_and_hybrid_hosts_use_the_named_systemd_hierarchy() {
        for prefix in ["", "0::/unrelated-unified-subtree\n"] {
            let membership = Membership::parse(format!("{prefix}3:cpu,cpuacct:/irrelevant\n7:name=systemd:/machine.slice/test.scope/init.scope\n").as_bytes()).unwrap();
            assert_eq!(membership.hierarchy, Hierarchy::Systemd(7));
            let group = group("/machine.slice/test.scope", Hierarchy::Systemd(7));
            group.verify_membership(&membership).unwrap();
            assert!(group
                .verify_membership(
                    &Membership::parse(b"8:name=systemd:/machine.slice/test.scope/init.scope\n")
                        .unwrap()
                )
                .is_err());
        }
    }

    #[test]
    fn ambiguous_missing_or_outside_namespace_membership_is_rejected() {
        for bytes in [
            b"".as_slice(),
            b"1:cpu:/machine\n",
            b"0::/machine\n0::/other\n",
            b"1:name=systemd:/machine\n2:name=systemd:/other\n",
            b"0::/../machine\n",
            b"0::relative\n",
            b"0::/machine/./service\n",
            b"0::/machine\0/service\n",
            b"invalid::/machine\n",
            b"0:\n",
        ] {
            assert!(Membership::parse(bytes).is_err(), "{bytes:?}");
        }
    }
}
