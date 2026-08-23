//! One-time command execution routing.
//!
//! When the program starts, [`ExecutionContext::new`] inspects the
//! [`PermissionLevel`] and optional [`ElevatedDaemon`] to construct the
//! correct command backend and typed stores. After that, consumers never
//! need to match on the daemon again.

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::platform::nvidia::NvidiaStateStore;
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::adapters::rootfs::RootfsStore;
use crate::adapters::runtime::inspection::MachineInspectionStore;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::HostOperationTracker;
use crate::composition::PermissionLevel;
use std::sync::Arc;

/// Bundled execution context. Generic commands always execute in the caller;
/// privileged mutations are available only through typed stores.
#[derive(Clone)]
pub struct ExecutionContext {
    pub local_cmd: Arc<dyn CommandRunner>,
    pub system_operations: SystemOperationStore,
    pub machine_inspection: MachineInspectionStore,
    pub nspawn: NspawnConfigStore,
    pub systemd_unit: SystemdUnitStore,
    pub rootfs: RootfsStore,
    pub nvidia_state: NvidiaStateStore,
    pub(crate) trusted_state_root: TrustedStateRoot,
    pub host_operations: HostOperationTracker,
    permission_level: PermissionLevel,
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl ExecutionContext {
    /// One-time routing.  `level` determines whether the daemon paths
    /// are used; `daemon` must be `Some` when `level` is `Elevated`.
    pub fn new(
        level: PermissionLevel,
        daemon: Option<Arc<ElevatedDaemon>>,
    ) -> Result<Self, ExecutionContextError> {
        validate_execution_mode(level, daemon.is_some())?;

        let local_cmd: Arc<dyn CommandRunner> = Arc::new(DefaultCommandRunner);
        let system_operations = SystemOperationStore::new(local_cmd.clone(), daemon.clone());
        let machine_inspection = MachineInspectionStore::new(daemon.clone());
        let (nspawn, systemd_unit) = match daemon.as_ref() {
            Some(daemon) => (
                NspawnConfigStore::elevated(Arc::clone(daemon)),
                SystemdUnitStore::elevated(Arc::clone(daemon)),
            ),
            None => (NspawnConfigStore::direct(), SystemdUnitStore::direct()),
        };
        let rootfs = match daemon.as_ref() {
            Some(daemon) => RootfsStore::elevated(Arc::clone(daemon)),
            None => RootfsStore::direct(),
        };
        let trusted_state_root = TrustedStateRoot::production();
        let nvidia_state = NvidiaStateStore::new(daemon.clone(), trusted_state_root.clone());
        Ok(Self {
            local_cmd,
            system_operations,
            machine_inspection,
            nspawn,
            systemd_unit,
            rootfs,
            nvidia_state,
            trusted_state_root,
            host_operations: HostOperationTracker::default(),
            permission_level: level,
            daemon,
        })
    }

    /// Expose the daemon reference for callers that need daemon-specific
    /// operations (DBus proxy, terminal spawning, shutdown).
    pub fn daemon_ref(&self) -> Option<&Arc<ElevatedDaemon>> {
        self.daemon.as_ref()
    }

    /// Shut down the daemon gracefully if one is running.
    pub async fn exit_daemon(&self) {
        if let Some(d) = &self.daemon {
            d.exit().await;
        }
    }
}

fn validate_execution_mode(
    level: PermissionLevel,
    has_daemon: bool,
) -> Result<(), ExecutionContextError> {
    match (level, has_daemon) {
        (PermissionLevel::Elevated, false) => Err(ExecutionContextError::MissingElevatedDaemon),
        (PermissionLevel::Root | PermissionLevel::User, true) => {
            Err(ExecutionContextError::UnexpectedElevatedDaemon { level })
        }
        _ => Ok(()),
    }
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("nspawn", &self.nspawn)
            .field("system_operations", &self.system_operations)
            .field("machine_inspection", &self.machine_inspection)
            .field("systemd_unit", &self.systemd_unit)
            .field("rootfs", &self.rootfs)
            .field("nvidia_state", &self.nvidia_state)
            .field("trusted_state_root", &self.trusted_state_root)
            .field("host_operations", &self.host_operations)
            .field("permission_level", &self.permission_level)
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionContextError {
    #[error("elevated execution requires an elevated daemon")]
    MissingElevatedDaemon,
    #[error("{level:?} execution must not receive an elevated daemon")]
    UnexpectedElevatedDaemon { level: PermissionLevel },
}

impl PartialEq for ExecutionContext {
    fn eq(&self, other: &Self) -> bool {
        self.daemon == other.daemon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_mode_requires_exact_daemon_pairing() {
        assert!(validate_execution_mode(PermissionLevel::User, false).is_ok());
        assert!(validate_execution_mode(PermissionLevel::Root, false).is_ok());
        assert!(matches!(
            validate_execution_mode(PermissionLevel::Elevated, false),
            Err(ExecutionContextError::MissingElevatedDaemon)
        ));
        assert!(matches!(
            validate_execution_mode(PermissionLevel::User, true),
            Err(ExecutionContextError::UnexpectedElevatedDaemon { .. })
        ));
        assert!(matches!(
            validate_execution_mode(PermissionLevel::Root, true),
            Err(ExecutionContextError::UnexpectedElevatedDaemon { .. })
        ));
        assert!(validate_execution_mode(PermissionLevel::Elevated, true).is_ok());
    }
}
