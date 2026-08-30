//! Route-fixed CLI machine inspection.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::application::operations::ExecutionRoute;
use crate::domain::inspection::MachineProperties;
use crate::domain::runtime::MachineEntry;
use std::path::PathBuf;
use std::sync::Arc;

/// Routes explicit machine inspection without exposing the daemon transport to
/// the application service.
#[derive(Clone)]
pub struct MachineInspectionStore {
    executor: Arc<dyn MachineInspectionExecutor>,
}

impl MachineInspectionStore {
    pub(crate) fn direct() -> Self {
        Self {
            executor: Arc::new(DirectMachineInspectionExecutor),
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            executor: Arc::new(ElevatedMachineInspectionExecutor { daemon }),
        }
    }

    pub async fn inspect(&self, name: &str, entry: &MachineEntry) -> Result<MachineProperties> {
        self.executor.inspect(name, entry).await
    }

    pub(crate) fn route(&self) -> ExecutionRoute {
        self.executor.route()
    }
}

impl std::fmt::Debug for MachineInspectionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineInspectionStore")
            .field("route", &self.executor.route())
            .finish()
    }
}

#[async_trait::async_trait]
trait MachineInspectionExecutor: Send + Sync + 'static {
    fn route(&self) -> ExecutionRoute;

    async fn inspect(&self, name: &str, entry: &MachineEntry) -> Result<MachineProperties>;
}

struct DirectMachineInspectionExecutor;

#[async_trait::async_trait]
impl MachineInspectionExecutor for DirectMachineInspectionExecutor {
    fn route(&self) -> ExecutionRoute {
        ExecutionRoute::LocalCli
    }

    async fn inspect(&self, name: &str, entry: &MachineEntry) -> Result<MachineProperties> {
        crate::adapters::runtime::state::inspect(name, entry).await
    }
}

struct ElevatedMachineInspectionExecutor {
    daemon: Arc<ElevatedDaemon>,
}

#[async_trait::async_trait]
impl MachineInspectionExecutor for ElevatedMachineInspectionExecutor {
    fn route(&self) -> ExecutionRoute {
        ExecutionRoute::ElevatedCli
    }

    async fn inspect(&self, name: &str, _entry: &MachineEntry) -> Result<MachineProperties> {
        self.daemon
            .cli_inspect_machine(name)
            .await
            .map_err(|error| NspawnError::Io(PathBuf::from("elevated CLI inspection"), error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::MachineState;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingInspector {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl MachineInspectionExecutor for RecordingInspector {
        fn route(&self) -> ExecutionRoute {
            ExecutionRoute::LocalCli
        }

        async fn inspect(&self, name: &str, entry: &MachineEntry) -> Result<MachineProperties> {
            assert_eq!(name, "test-machine");
            assert_eq!(entry.name, "test-machine");
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MachineProperties::default())
        }
    }

    #[tokio::test]
    async fn store_delegates_to_its_fixed_executor() {
        let executor = Arc::new(RecordingInspector {
            calls: AtomicUsize::new(0),
        });
        let store = MachineInspectionStore {
            executor: executor.clone(),
        };
        let entry = MachineEntry {
            name: "test-machine".into(),
            class: MachineEntry::NSPAWN_CLASS.into(),
            service: MachineEntry::NSPAWN_SERVICE.into(),
            state: MachineState::Running,
            address: None,
            all_addresses: Vec::new(),
        };

        store.inspect("test-machine", &entry).await.unwrap();

        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.route(), ExecutionRoute::LocalCli);
    }
}
