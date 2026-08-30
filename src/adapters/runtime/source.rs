use crate::domain::runtime::{ImageEntry, MachineEntry, RuntimeSnapshot, StatusUpdate};
use crate::nspawn::errors::Result;
use crate::nspawn::models::MachineProperties;

/// Read-only runtime discovery, inspection, and observation.
///
/// Application services depend on this port without gaining mutation methods.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait RuntimeSource: Send + Sync + 'static {
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> Result<Vec<MachineEntry>>;
    async fn list_images(&self) -> Result<Vec<ImageEntry>>;
    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        let (machines, images) = tokio::try_join!(self.list_machines(), self.list_images())?;
        Ok(RuntimeSnapshot::new(machines, images))
    }
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()>;
}
