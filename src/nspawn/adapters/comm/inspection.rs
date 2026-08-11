//! Mode-aware CLI machine inspection.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use crate::nspawn::sys::daemon::ElevatedDaemon;
use std::path::PathBuf;
use std::sync::Arc;

/// Routes explicit CLI inspection without exposing the daemon transport to the
/// application service. Elevated mode asks the root daemon for the complete
/// fixed command view; direct mode stays on readable runtime registration data.
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
}

impl std::fmt::Debug for MachineInspectionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineInspectionStore")
            .field("elevated", &self.daemon.is_some())
            .finish()
    }
}
