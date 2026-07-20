//! One-time command execution routing.
//!
//! When the program starts, [`ExecutionContext::new`] inspects the
//! [`PermissionLevel`] and optional [`ElevatedDaemon`] to construct the
//! correct command backend and typed stores. After that, consumers never
//! need to match on the daemon again.

use crate::nspawn::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::nspawn::adapters::rootfs::RootfsStore;
use crate::nspawn::adapters::storage::ManagedStorageStore;
use crate::nspawn::ops::provision::{BootstrapStore, ImageImportStore, OciPullStore};
use crate::nspawn::ops::PermissionLevel;
use crate::nspawn::platform::nvidia::NvidiaStateStore;
use crate::nspawn::sys::command::{CommandRunner, DefaultCommandRunner};
use crate::nspawn::sys::daemon::{DaemonCommandRunner, ElevatedDaemon};
use std::sync::Arc;

/// Bundled execution context. Privileged mutations use typed stores; the
/// remaining command runner is retained only for operations not yet migrated.
#[derive(Clone)]
pub struct ExecutionContext {
    pub cmd: Arc<dyn CommandRunner>,
    pub nspawn: NspawnConfigStore,
    pub systemd_unit: SystemdUnitStore,
    pub rootfs: RootfsStore,
    pub bootstrap: BootstrapStore,
    pub image_import: ImageImportStore,
    pub oci_pull: OciPullStore,
    pub managed_storage: ManagedStorageStore,
    pub nvidia_state: NvidiaStateStore,
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl ExecutionContext {
    /// One-time routing.  `level` determines whether the daemon paths
    /// are used; `daemon` must be `Some` when `level` is `Elevated`.
    pub fn new(_level: PermissionLevel, daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        let cmd: Arc<dyn CommandRunner> = match &daemon {
            Some(d) => Arc::new(DaemonCommandRunner::new(d.clone())),
            None => Arc::new(DefaultCommandRunner),
        };
        let nspawn = NspawnConfigStore::new(daemon.clone());
        let systemd_unit = SystemdUnitStore::new(daemon.clone());
        let rootfs = RootfsStore::new(daemon.clone());
        let bootstrap = BootstrapStore::new(cmd.clone(), daemon.clone());
        let image_import = ImageImportStore::new(daemon.clone());
        let oci_pull = OciPullStore::new(cmd.clone(), daemon.clone());
        let managed_storage = ManagedStorageStore::new(daemon.clone());
        let nvidia_state = NvidiaStateStore::new(daemon.clone());
        Self {
            cmd,
            nspawn,
            systemd_unit,
            rootfs,
            bootstrap,
            image_import,
            oci_pull,
            managed_storage,
            nvidia_state,
            daemon,
        }
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

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("nspawn", &self.nspawn)
            .field("systemd_unit", &self.systemd_unit)
            .field("rootfs", &self.rootfs)
            .field("bootstrap", &"BootstrapStore")
            .field("image_import", &"ImageImportStore")
            .field("oci_pull", &"OciPullStore")
            .field("managed_storage", &self.managed_storage)
            .field("nvidia_state", &self.nvidia_state)
            .field("daemon", &self.daemon)
            .finish()
    }
}

impl PartialEq for ExecutionContext {
    fn eq(&self, other: &Self) -> bool {
        self.daemon == other.daemon
    }
}
