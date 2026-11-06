mod cgroup;
mod runtime;

pub use runtime::{MachineRegistration, MachineTargetSource};

use crate::process::ProcessHandle;
use crate::WorkerProcess;
use cgroup::MachineCgroup;
use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
#[cfg(test)]
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Host-selected machine instance retained throughout helper preparation.
/// Neither the leader nor its unit cgroup may come from guest protocol data.
pub struct MachineTarget {
    leader: ProcessHandle,
    cgroup: MachineCgroup,
    reader_cgroup_namespace: File,
}

impl MachineTarget {
    /// Retain the source's pinned leader across the unit queries. A restart
    /// invalidates acquisition even if the registered name, PID and unit recur.
    pub async fn resolve(source: &impl MachineTargetSource, machine: &str) -> io::Result<Self> {
        let reader_cgroup_namespace = File::open("/proc/thread-self/ns/cgroup")?;
        let before = source.registration(machine).await?;
        if before.name() != machine {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime returned a registration for a different machine",
            ));
        }
        before.leader.recheck()?;
        let cgroup = source.control_group(before.unit()).await?;
        let after = source.registration(machine).await?;
        before.verify_same_instance(&after)?;
        if source.control_group(before.unit()).await? != cgroup {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "machine unit cgroup changed during target acquisition",
            ));
        }
        Self::new(before.leader, &cgroup, reader_cgroup_namespace)
    }

    fn new(
        leader: ProcessHandle,
        control_group: &Path,
        reader_cgroup_namespace: File,
    ) -> io::Result<Self> {
        let cgroup = MachineCgroup::new(control_group, &leader)?;
        let target = Self {
            leader,
            cgroup,
            reader_cgroup_namespace,
        };
        target.recheck()?;
        Ok(target)
    }

    #[cfg(test)]
    pub(crate) fn from_leader(leader_pidfd: OwnedFd, control_group: &Path) -> io::Result<Self> {
        Self::new(
            ProcessHandle::open(leader_pidfd)?,
            control_group,
            File::open("/proc/thread-self/ns/cgroup")?,
        )
    }

    pub fn leader_pid(&self) -> i32 {
        self.leader.pid()
    }

    pub fn root(&self) -> BorrowedFd<'_> {
        self.leader.root()
    }

    /// Recheck the pinned leader, including membership in its hosting unit.
    /// A new process with the same name or PID cannot revive this target.
    pub fn recheck(&self) -> io::Result<()> {
        self.verify_reader_namespace()?;
        self.leader.recheck()?;
        self.cgroup.verify(&self.leader)?;
        self.leader.recheck()
    }

    pub fn verify_worker(&self, worker: &WorkerProcess) -> io::Result<()> {
        self.verify_process(worker.handle())
    }

    fn verify_process(&self, process: &ProcessHandle) -> io::Result<()> {
        self.recheck()?;
        process.recheck()?;
        process.verify_location(&self.leader)?;
        self.cgroup.verify(process)?;
        process.recheck()?;
        self.recheck()
    }

    fn verify_reader_namespace(&self) -> io::Result<()> {
        let expected = self.reader_cgroup_namespace.metadata()?;
        let actual = std::fs::metadata("/proc/thread-self/ns/cgroup")?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host cgroup namespace changed during preparation",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
