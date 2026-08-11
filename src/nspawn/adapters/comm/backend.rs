use crate::nspawn::errors::Result;
use crate::nspawn::models::{
    AllowedSignal, ContainerEntry, ImageEntry, MachineProperties, RuntimeSnapshot, StatusUpdate,
};

/// Unified backend for communicating with systemd-machined.
///
/// DBus observers publish dirty hints from signals, while CLI observers
/// publish complete snapshots from polling. Consumers do not need to repeat
/// discovery when a backend already produced a snapshot.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ContainerBackend: Send + Sync + 'static {
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>>;
    /// Return persistent machine images independently from running machines.
    async fn list_images(&self) -> Result<Vec<ImageEntry>>;
    /// Return one normalized machine/image view from this backend.
    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        let (machines, images) = tokio::try_join!(self.list_machines(), self.list_images())?;
        Ok(RuntimeSnapshot::new(machines, images))
    }
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<()>;
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
    async fn reload_daemon(&self) -> Result<()>;
    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()>;
}
