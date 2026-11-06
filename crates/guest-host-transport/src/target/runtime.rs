use crate::process::ProcessHandle;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

/// Host registration evidence bound to an already pinned leader. This owner
/// cannot be reconstructed from a guest packet or cloned as a numeric PID.
pub struct MachineRegistration {
    pub(super) name: String,
    pub(super) unit: String,
    pub(super) leader: ProcessHandle,
    leader_start_time: NonZeroU64,
}

impl MachineRegistration {
    /// `leader_start_time` is the host procfs observation associated with this
    /// registration, taken before acquiring `leader_pidfd`. Reading it only
    /// after pidfd_open would let a replacement process supply both values.
    /// The source must recheck the runtime registration around this acquisition;
    /// the constructor checks the supplied process evidence, not that relation.
    pub fn new(
        name: String,
        unit: String,
        leader_pid: u32,
        leader_start_time: NonZeroU64,
        leader_pidfd: OwnedFd,
    ) -> io::Result<Self> {
        if name.is_empty()
            || leader_pid == 0
            || leader_pid > i32::MAX as u32
            || !(unit.ends_with(".scope") || unit.ends_with(".service"))
            || unit
                .bytes()
                .any(|byte| byte == b'/' || byte.is_ascii_whitespace() || byte == 0)
        {
            return Err(invalid(
                "runtime did not return a valid machine registration and hosting unit",
            ));
        }
        let leader = ProcessHandle::open(leader_pidfd)?;
        if leader.pid() as u32 != leader_pid {
            return Err(invalid(
                "leader pidfd does not match the registered leader PID",
            ));
        }
        if leader.start_time()? != leader_start_time {
            return Err(invalid(
                "leader pidfd does not match the registered leader start time",
            ));
        }
        Ok(Self {
            name,
            unit,
            leader,
            leader_start_time,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn unit(&self) -> &str {
        &self.unit
    }

    pub fn leader_pid(&self) -> u32 {
        self.leader.pid() as u32
    }

    pub(super) fn verify_same_instance(&self, other: &Self) -> io::Result<()> {
        self.leader.recheck()?;
        other.leader.recheck()?;
        if self.name != other.name
            || self.unit != other.unit
            || self.leader.pid() != other.leader.pid()
            || self.leader_start_time != other.leader_start_time
        {
            return Err(invalid(
                "machine registration changed during target acquisition",
            ));
        }
        // Both pidfds must still be live. Equal numeric PIDs alone would allow
        // a later registration to replace the first process after it exits.
        self.leader.recheck()
    }
}

/// Host runtime queries used to acquire a target. The source owns the binding
/// between a machine's registration and its process evidence; returning a
/// pidfd obtained from an unverified, stale Leader property is insufficient.
///
/// Implementations read the actual Unit and that unit's ControlGroup, without
/// guessing a unit name or accepting either value from the guest. ControlGroup
/// must use the acquiring thread's cgroup namespace. Each query has a deadline;
/// acquisition propagates failures without fallback.
pub trait MachineTargetSource {
    fn registration(
        &self,
        machine: &str,
    ) -> impl Future<Output = io::Result<MachineRegistration>> + Send;

    fn control_group(&self, unit: &str) -> impl Future<Output = io::Result<PathBuf>> + Send;
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
