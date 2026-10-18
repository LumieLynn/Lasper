use super::RuntimeResult;
use crate::application::operations::{ExecutionRoute, RouteFallback};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, RuntimeSnapshot};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeQuery<T> {
    pub value: T,
    pub route: ExecutionRoute,
    pub fallback: Option<RouteFallback>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeUpdate {
    Snapshot(RuntimeQuery<RuntimeSnapshot>),
    BackendFailure {
        message: String,
        consecutive_failures: u32,
    },
}

/// One-shot reads. Observation never gives an adapter its own snapshot cache.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub(crate) trait RuntimePort: Send + Sync + 'static {
    fn snapshot_route(&self) -> ExecutionRoute;
    fn inspection_route(&self) -> ExecutionRoute;
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>>;
    async fn snapshot(&self) -> RuntimeResult<RuntimeSnapshot>;
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> RuntimeResult<MachineProperties>;
    async fn inspect_unit(&self, machine: &MachineName) -> RuntimeResult<MachineProperties>;
}

/// Change hints from the host, independent of querying or polling the catalog.
/// Dropping the future must release its subscriptions and watchers.
#[async_trait::async_trait]
pub(crate) trait RuntimeChangePort: Send + Sync + 'static {
    async fn watch_changes(&self, changed: watch::Sender<()>) -> RuntimeResult<()>;
}
