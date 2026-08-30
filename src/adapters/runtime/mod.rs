//! RuntimeCatalog composition over the current host adapters.

pub(crate) mod cli;
pub(crate) mod dbus;
pub(crate) mod elevated;
pub(crate) mod formatting;
pub(crate) mod inspection;
pub(crate) mod source;
pub(crate) mod state;

use crate::adapters::error::NspawnError;
use crate::adapters::runtime::cli::CliBackend;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::elevated::DaemonBackend;
use crate::adapters::runtime::inspection::MachineInspectionStore;
use crate::adapters::runtime::source::RuntimeSource;
use crate::application::operations::ExecutionRoute;
use crate::application::runtime::{RuntimeCatalog, RuntimeError, RuntimePort, RuntimeResult};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, RuntimeSnapshot, StatusUpdate};
use std::sync::Arc;

pub(crate) fn compose_runtime_catalog(
    local_cmd: Arc<dyn crate::adapters::process::CommandRunner>,
    fallback_inspector: Option<MachineInspectionStore>,
    primary_route: PrimaryRuntimeRoute,
) -> Arc<RuntimeCatalog> {
    let (nudge_tx, nudge_rx) = tokio::sync::watch::channel(());
    let cli = CliBackend::new(local_cmd);
    cli.set_nudge(nudge_rx);
    let fallback_source: Arc<dyn RuntimeSource> = Arc::new(cli);
    let fallback: Arc<dyn RuntimePort> = Arc::new(SourceRuntimePort {
        source: fallback_source,
        inspection_route: fallback_inspector
            .as_ref()
            .map(MachineInspectionStore::route)
            .unwrap_or(ExecutionRoute::LocalCli),
        inspector: fallback_inspector
            .map(RuntimeInspector::Store)
            .unwrap_or(RuntimeInspector::Source),
        snapshot_route: ExecutionRoute::LocalCli,
    });

    let primary = match primary_route {
        PrimaryRuntimeRoute::Disabled => None,
        PrimaryRuntimeRoute::DirectDbus(dbus) => Some((
            Arc::new(dbus) as Arc<dyn RuntimeSource>,
            ExecutionRoute::DirectDbus,
        )),
        PrimaryRuntimeRoute::ElevatedDbus(daemon) => Some((
            Arc::new(DaemonBackend::new(daemon)) as Arc<dyn RuntimeSource>,
            ExecutionRoute::ElevatedDbus,
        )),
    }
    .map(|(source, route)| {
        Arc::new(SourceRuntimePort {
            source,
            inspector: RuntimeInspector::Source,
            snapshot_route: route,
            inspection_route: route,
        }) as Arc<dyn RuntimePort>
    });

    Arc::new(RuntimeCatalog::new(
        primary,
        fallback,
        vec![
            crate::paths::machines_dir(),
            crate::paths::runtime_machines_dir(),
        ],
        Some(nudge_tx),
    ))
}

pub(crate) enum PrimaryRuntimeRoute {
    Disabled,
    DirectDbus(DbusBackend),
    ElevatedDbus(Arc<crate::adapters::elevated::ElevatedDaemon>),
}

enum RuntimeInspector {
    Source,
    Store(MachineInspectionStore),
}

struct SourceRuntimePort {
    source: Arc<dyn RuntimeSource>,
    inspector: RuntimeInspector,
    snapshot_route: ExecutionRoute,
    inspection_route: ExecutionRoute,
}

#[async_trait::async_trait]
impl RuntimePort for SourceRuntimePort {
    fn snapshot_route(&self) -> ExecutionRoute {
        self.snapshot_route
    }

    fn inspection_route(&self) -> ExecutionRoute {
        self.inspection_route
    }

    async fn is_available(&self) -> bool {
        self.source.is_available().await
    }

    async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>> {
        self.source.list_machines().await.map_err(map_runtime_error)
    }

    async fn snapshot(&self) -> RuntimeResult<RuntimeSnapshot> {
        self.source.snapshot().await.map_err(map_runtime_error)
    }

    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> RuntimeResult<MachineProperties> {
        match &self.inspector {
            RuntimeInspector::Source => self
                .source
                .get_properties(machine.as_str(), entry.access().is_nspawn())
                .await
                .map_err(map_runtime_error),
            RuntimeInspector::Store(store) => store
                .inspect(machine.as_str(), entry)
                .await
                .map_err(map_runtime_error),
        }
    }

    async fn watch(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> RuntimeResult<()> {
        self.source
            .watch_events(tx)
            .await
            .map_err(map_runtime_error)
    }
}

pub(crate) fn map_runtime_error(error: NspawnError) -> RuntimeError {
    let message = error.to_string();
    if error.is_polkit_rejection() {
        RuntimeError::permission_denied(message)
    } else {
        RuntimeError::failed(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{MachineClass, MachineProvider, MachineState};

    #[tokio::test]
    async fn foreign_runtime_identity_disables_nspawn_unit_inspection() {
        let mut source = crate::adapters::runtime::source::MockRuntimeSource::new();
        source
            .expect_get_properties()
            .withf(|name, include_nspawn_unit| name == "guest-vm" && !include_nspawn_unit)
            .once()
            .returning(|_, _| Ok(MachineProperties::default()));
        let port = SourceRuntimePort {
            source: Arc::new(source),
            inspector: RuntimeInspector::Source,
            snapshot_route: ExecutionRoute::DirectDbus,
            inspection_route: ExecutionRoute::DirectDbus,
        };
        let entry = MachineEntry {
            name: "guest-vm".into(),
            class: MachineClass::Vm,
            service: MachineProvider::Vmspawn,
            state: MachineState::Running,
            addresses: Default::default(),
        };

        port.inspect(&MachineName::new("guest-vm").unwrap(), &entry)
            .await
            .unwrap();
    }
}
