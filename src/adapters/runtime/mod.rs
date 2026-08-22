//! RuntimeCatalog composition over the current host adapters.

pub(crate) mod cli;
pub(crate) mod dbus;
pub(crate) mod elevated;
pub(crate) mod formatting;
pub(crate) mod inspection;
pub(crate) mod source;
pub(crate) mod state;

use crate::adapters::runtime::cli::CliBackend;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::elevated::DaemonBackend;
use crate::adapters::runtime::inspection::MachineInspectionStore;
use crate::adapters::runtime::source::RuntimeSource;
use crate::application::operations::ExecutionRoute;
use crate::application::runtime::{RuntimeCatalog, RuntimePort};
use crate::composition::ExecutionContext;
use crate::composition::PermissionLevel;
use crate::nspawn::errors::Result;
use crate::nspawn::models::{
    ContainerEntry, MachineName, MachineProperties, RuntimeSnapshot, StatusUpdate,
};
use std::sync::Arc;

pub(crate) fn compose_runtime_catalog(
    level: PermissionLevel,
    cli_mode: bool,
    exec_ctx: &ExecutionContext,
) -> Arc<RuntimeCatalog> {
    let (nudge_tx, nudge_rx) = tokio::sync::watch::channel(());
    let cli = CliBackend::new(exec_ctx.local_cmd.clone());
    cli.set_nudge(nudge_rx);
    let fallback_source: Arc<dyn RuntimeSource> = Arc::new(cli);
    let fallback: Arc<dyn RuntimePort> = Arc::new(SourceRuntimePort {
        source: fallback_source,
        inspector: if cli_mode {
            RuntimeInspector::Store(exec_ctx.machine_inspection.clone())
        } else {
            RuntimeInspector::Source
        },
        snapshot_route: ExecutionRoute::LocalCli,
        inspection_route: if cli_mode && level == PermissionLevel::Elevated {
            ExecutionRoute::ElevatedCli
        } else {
            ExecutionRoute::LocalCli
        },
    });

    let primary = if cli_mode {
        None
    } else {
        let (source, route): (Arc<dyn RuntimeSource>, ExecutionRoute) = match level {
            PermissionLevel::Elevated => {
                let backend = DaemonBackend::new(
                    exec_ctx
                        .daemon_ref()
                        .cloned()
                        .expect("elevated runtime catalog requires daemon"),
                );
                (Arc::new(backend), ExecutionRoute::ElevatedDbus)
            }
            PermissionLevel::User | PermissionLevel::Root => {
                (Arc::new(DbusBackend::new()), ExecutionRoute::DirectDbus)
            }
        };
        Some(Arc::new(SourceRuntimePort {
            source,
            inspector: RuntimeInspector::Source,
            snapshot_route: route,
            inspection_route: route,
        }) as Arc<dyn RuntimePort>)
    };

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

    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        self.source.list_machines().await
    }

    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        self.source.snapshot().await
    }

    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &ContainerEntry,
    ) -> Result<MachineProperties> {
        match &self.inspector {
            RuntimeInspector::Source => self.source.get_properties(machine.as_str()).await,
            RuntimeInspector::Store(store) => store.inspect(machine.as_str(), entry).await,
        }
    }

    async fn watch(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        self.source.watch_events(tx).await
    }
}
