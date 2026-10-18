//! RuntimeCatalog composition over the current host adapters.

pub(crate) mod dbus;
pub(crate) mod elevated;
mod filesystem;
pub(crate) mod source;
pub(crate) mod state;
pub(crate) mod systemd_properties;
pub(crate) mod systemd_tools;

use crate::adapters::error::NspawnError;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::elevated::DaemonBackend;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::runtime::systemd_tools::SystemdToolsBackend;
use crate::application::operations::ExecutionRoute;
use crate::application::runtime::{
    RuntimeCatalog, RuntimeChangePort, RuntimeError, RuntimeObservers, RuntimePort, RuntimeResult,
};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, RuntimeSnapshot};
use std::sync::Arc;

pub(crate) fn compose_runtime_catalog(
    systemd_tools: SystemdToolsBackend,
    primary_route: PrimaryRuntimeRoute,
) -> Arc<RuntimeCatalog> {
    let property_query_route = systemd_tools.property_query_route();
    let fallback_source: Arc<dyn RuntimeSource> = Arc::new(systemd_tools);
    let fallback: Arc<dyn RuntimePort> = Arc::new(SourceRuntimePort {
        source: fallback_source,
        property_query_route,
        snapshot_route: ExecutionRoute::LocalSystemdTools,
    });

    let (primary, backend): (_, Option<Arc<dyn RuntimeChangePort>>) = match primary_route {
        PrimaryRuntimeRoute::Disabled => (None, None),
        PrimaryRuntimeRoute::DirectDbus(dbus) => {
            let dbus = Arc::new(dbus);
            (
                Some((
                    Arc::clone(&dbus) as Arc<dyn RuntimeSource>,
                    ExecutionRoute::DirectDbus,
                )),
                Some(dbus),
            )
        }
        PrimaryRuntimeRoute::ElevatedDbus(daemon) => {
            let daemon = Arc::new(DaemonBackend::new(daemon));
            (
                Some((
                    Arc::clone(&daemon) as Arc<dyn RuntimeSource>,
                    ExecutionRoute::ElevatedDbus,
                )),
                Some(daemon),
            )
        }
    };
    let primary = primary.map(|(source, route)| {
        Arc::new(SourceRuntimePort {
            source,
            snapshot_route: route,
            property_query_route: route,
        }) as Arc<dyn RuntimePort>
    });

    Arc::new(RuntimeCatalog::new(
        primary,
        fallback,
        RuntimeObservers {
            backend,
            filesystem: Some(Arc::new(filesystem::FilesystemChanges::new(vec![
                crate::paths::machines_dir(),
                crate::paths::runtime_machines_dir(),
            ]))),
        },
    ))
}

pub(crate) enum PrimaryRuntimeRoute {
    Disabled,
    DirectDbus(DbusBackend),
    ElevatedDbus(Arc<crate::adapters::elevated::ElevatedDaemon>),
}

struct SourceRuntimePort {
    source: Arc<dyn RuntimeSource>,
    snapshot_route: ExecutionRoute,
    property_query_route: ExecutionRoute,
}

#[async_trait::async_trait]
impl RuntimePort for SourceRuntimePort {
    fn snapshot_route(&self) -> ExecutionRoute {
        self.snapshot_route
    }

    fn inspection_route(&self) -> ExecutionRoute {
        self.property_query_route
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
        self.source
            .get_properties(machine.as_str(), entry.access().is_nspawn())
            .await
            .map_err(map_runtime_error)
    }

    async fn inspect_unit(&self, machine: &MachineName) -> RuntimeResult<MachineProperties> {
        self.source
            .get_unit_properties(machine)
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
            snapshot_route: ExecutionRoute::DirectDbus,
            property_query_route: ExecutionRoute::DirectDbus,
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
