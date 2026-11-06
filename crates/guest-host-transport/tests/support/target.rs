use lasper_guest_host_transport::{MachineRegistration, MachineTarget, MachineTargetSource};
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;

pub async fn current_target() -> Arc<MachineTarget> {
    target_for(std::process::id()).await
}

pub async fn target_for(leader: u32) -> Arc<MachineTarget> {
    // The fixtures share this test process's namespaces and cgroup. This tests
    // resource matching, not discovery of an actual nspawn machine unit.
    let records = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let root = records
        .lines()
        .find_map(|line| {
            let mut fields = line.splitn(3, ':');
            fields.next()?;
            (fields.next()?.split(',').any(|name| name == "name=systemd"))
                .then(|| fields.next())
                .flatten()
        })
        .or_else(|| records.lines().find_map(|line| line.strip_prefix("0::")))
        .unwrap();
    Arc::new(
        MachineTarget::resolve(
            &FixtureSource {
                leader,
                root: root.into(),
            },
            "fixture",
        )
        .await
        .unwrap(),
    )
}

// Fixture-only runtime evidence. Production sources query machined/systemd;
// these tests deliberately use processes in the test runner's own cgroup.
pub struct FixtureSource {
    pub leader: u32,
    pub root: PathBuf,
}

impl MachineTargetSource for FixtureSource {
    async fn registration(&self, machine: &str) -> io::Result<MachineRegistration> {
        let bytes = std::fs::read(format!("/proc/{}/stat", self.leader))?;
        let end = bytes.iter().rposition(|byte| *byte == b')').unwrap();
        let start = std::str::from_utf8(&bytes[end + 1..])
            .unwrap()
            .split_ascii_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let observed = NonZeroU64::new(start).unwrap();
        // SAFETY: The fixture leader stays alive during this acquisition.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, self.leader, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pidfd_open returned a new descriptor without another owner.
        let leader = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        MachineRegistration::new(
            machine.into(),
            "fixture.scope".into(),
            self.leader,
            observed,
            leader,
        )
    }

    async fn control_group(&self, _: &str) -> io::Result<PathBuf> {
        Ok(self.root.clone())
    }
}
