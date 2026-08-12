//! Mode-aware CLI machine inspection.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use crate::nspawn::sys::daemon::ElevatedDaemon;
use std::path::PathBuf;
use std::sync::Arc;

/// Routes explicit machine inspection without exposing the daemon transport to
/// the application service.
#[derive(Clone)]
pub struct MachineInspectionStore {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl MachineInspectionStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub async fn inspect(&self, name: &str, entry: &ContainerEntry) -> Result<MachineProperties> {
        if let Some(daemon) = &self.daemon {
            daemon
                .cli_inspect_machine(name)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from("elevated CLI inspection"), error))
        } else {
            crate::nspawn::adapters::comm::runtime_state::inspect(name, entry).await
        }
    }

    /// Inspect systemd properties without requiring a machined runtime
    /// registration. This is used by the image inspector.
    pub async fn inspect_static(&self, name: &str) -> Result<MachineProperties> {
        if let Some(daemon) = &self.daemon {
            daemon
                .cli_inspect_machine(name)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from("elevated CLI inspection"), error))
        } else {
            crate::nspawn::adapters::comm::cli::get_properties_with_runner(
                name,
                &crate::nspawn::sys::command::DefaultCommandRunner,
            )
            .await
        }
    }
}

impl std::fmt::Debug for MachineInspectionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineInspectionStore")
            .field("elevated", &self.daemon.is_some())
            .finish()
    }
}
