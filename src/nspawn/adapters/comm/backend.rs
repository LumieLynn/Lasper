use crate::nspawn::errors::Result;
use crate::nspawn::models::{AllowedSignal, ContainerEntry, ImageEntry, MachineProperties};

/// Unified backend for communicating with systemd-machined.
///
/// Every method has both a DBus and CLI implementation.
/// The one exception is `watch_events` (DBus signals, no CLI equivalent),
/// which lives as an inherent method on `DbusBackend`.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ContainerBackend: Send + Sync + 'static {
    async fn is_available(&self) -> bool;
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>>;
    /// Return persistent machine images independently from running machines.
    async fn list_images(&self) -> Result<Vec<ImageEntry>>;
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
    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<()>) -> Result<()>;
}
