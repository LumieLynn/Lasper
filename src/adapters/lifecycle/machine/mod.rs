//! Compose preparation, control and observation for machine lifecycle use cases.

mod control;
mod observation;
mod preparation;

pub(crate) use control::{
    execute_systemd_tools_machine_runtime, execute_systemd_tools_nspawn_launch,
    execute_systemd_tools_nspawn_unit,
};
pub(crate) use preparation::DirectStartPreparation;

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::lifecycle::machine::control::{MachineControlRoute, RoutedMachineControl};
use crate::adapters::lifecycle::machine::observation::{
    CatalogMachineObservation, LocalStartDiagnostics,
};
use crate::adapters::lifecycle::machine::preparation::{
    RoutedStartPreparation, StartPreparationRoute,
};
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::machine_lifecycle::{MachineControlTransport, MachineLifecycleService};
use crate::application::{OperationRegistry, RuntimeCatalog};
use crate::domain::nvidia::NvidiaCdiSource;
use std::sync::Arc;

pub(crate) struct MachineLifecycleAdapters {
    pub(crate) local_cmd: Arc<dyn CommandRunner>,
    pub(crate) trusted_state_root: TrustedStateRoot,
    pub(crate) nvidia_cdi_source: NvidiaCdiSource,
    pub(crate) skip_nvidia_permission_denied: bool,
}

pub(crate) enum MachineLifecycleRoute {
    DirectDbus(DbusBackend),
    LocalSystemdTools,
    Elevated {
        daemon: Arc<ElevatedDaemon>,
        transport: MachineControlTransport,
    },
}

pub(crate) fn compose_machine_lifecycle(
    runtime: Arc<RuntimeCatalog>,
    registry: Arc<OperationRegistry>,
    route: MachineLifecycleRoute,
    adapters: MachineLifecycleAdapters,
) -> Arc<MachineLifecycleService> {
    let MachineLifecycleAdapters {
        local_cmd,
        trusted_state_root,
        nvidia_cdi_source,
        skip_nvidia_permission_denied,
    } = adapters;
    let (control_route, preparation_route) = match route {
        MachineLifecycleRoute::DirectDbus(dbus) => (
            MachineControlRoute::DirectDbus {
                dbus: dbus.clone(),
                local_runner: local_cmd.clone(),
            },
            StartPreparationRoute::Direct {
                host: DirectStartPreparation::new(
                    trusted_state_root,
                    nvidia_cdi_source,
                    skip_nvidia_permission_denied,
                ),
                system_operations: SystemOperationStore::dbus(dbus, local_cmd.clone()),
            },
        ),
        MachineLifecycleRoute::LocalSystemdTools => (
            MachineControlRoute::LocalSystemdTools {
                runner: local_cmd.clone(),
            },
            StartPreparationRoute::Direct {
                host: DirectStartPreparation::new(
                    trusted_state_root,
                    nvidia_cdi_source,
                    skip_nvidia_permission_denied,
                ),
                system_operations: SystemOperationStore::direct(local_cmd.clone()),
            },
        ),
        MachineLifecycleRoute::Elevated { daemon, transport } => (
            MachineControlRoute::Daemon {
                daemon: daemon.clone(),
                transport,
            },
            StartPreparationRoute::Elevated {
                daemon,
                nvidia_cdi_source,
                transport,
            },
        ),
    };
    Arc::new(MachineLifecycleService::new(
        Arc::new(RoutedMachineControl {
            route: control_route,
        }),
        Arc::new(RoutedStartPreparation {
            route: preparation_route,
            runtime: runtime.clone(),
        }),
        Arc::new(CatalogMachineObservation {
            runtime,
            systemd_tools: local_cmd.clone(),
        }),
        Arc::new(LocalStartDiagnostics { runner: local_cmd }),
        registry,
    ))
}
