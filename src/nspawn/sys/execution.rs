//! One-time command execution routing.
//!
//! When the program starts, [`ExecutionContext::new`] inspects the
//! [`PermissionLevel`] and optional [`ElevatedDaemon`] to construct the
//! correct [`CommandRunner`] and [`ElevatedIo`].  After that, consumers
//! never need to match on the daemon again — they just use typed stores
//! or the remaining low-level `ctx.cmd`/`ctx.io` surfaces.

use crate::nspawn::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::nspawn::adapters::rootfs::RootfsStore;
use crate::nspawn::adapters::storage::ManagedStorageStore;
use crate::nspawn::ops::PermissionLevel;
use crate::nspawn::platform::nvidia::{NvidiaStagingStore, NvidiaStateStore};
use crate::nspawn::sys::command::{CommandRunner, DefaultCommandRunner};
use crate::nspawn::sys::daemon::{DaemonCommandRunner, ElevatedDaemon};
use crate::nspawn::sys::elevated_io::ElevatedIo;
use std::sync::Arc;

/// Bundled execution context — command execution and file I/O, both
/// routed through the daemon when elevated or through direct system
/// paths otherwise.
#[derive(Clone)]
pub struct ExecutionContext {
    pub cmd: Arc<dyn CommandRunner>,
    pub io: ElevatedIo,
    pub nspawn: NspawnConfigStore,
    pub systemd_unit: SystemdUnitStore,
    pub rootfs: RootfsStore,
    pub managed_storage: ManagedStorageStore,
    pub nvidia_state: NvidiaStateStore,
    pub nvidia_staging: NvidiaStagingStore,
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl ExecutionContext {
    /// One-time routing.  `level` determines whether the daemon paths
    /// are used; `daemon` must be `Some` when `level` is `Elevated`.
    pub fn new(level: PermissionLevel, daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        let cmd: Arc<dyn CommandRunner> = match &daemon {
            Some(d) => Arc::new(DaemonCommandRunner::new(d.clone())),
            None => Arc::new(DefaultCommandRunner),
        };
        let io = match &daemon {
            Some(d) => ElevatedIo::with_daemon(level, d.clone()),
            None => ElevatedIo::new(level),
        };
        let nspawn = NspawnConfigStore::new(daemon.clone());
        let systemd_unit = SystemdUnitStore::new(daemon.clone());
        let rootfs = RootfsStore::new(daemon.clone());
        let managed_storage = ManagedStorageStore::new(daemon.clone());
        let nvidia_state = NvidiaStateStore::new(daemon.clone());
        let nvidia_staging = NvidiaStagingStore::new(daemon.clone());
        Self {
            cmd,
            io,
            nspawn,
            systemd_unit,
            rootfs,
            managed_storage,
            nvidia_state,
            nvidia_staging,
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
            .field("io", &self.io)
            .field("nspawn", &self.nspawn)
            .field("systemd_unit", &self.systemd_unit)
            .field("rootfs", &self.rootfs)
            .field("managed_storage", &self.managed_storage)
            .field("nvidia_state", &self.nvidia_state)
            .field("nvidia_staging", &self.nvidia_staging)
            .field("daemon", &self.daemon)
            .finish()
    }
}

impl PartialEq for ExecutionContext {
    fn eq(&self, other: &Self) -> bool {
        self.daemon == other.daemon
    }
}
