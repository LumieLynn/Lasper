use super::errors::Result;
use super::machinectl::ContainerEntry;

use async_trait::async_trait;

#[async_trait]
pub trait NspawnManager: Send + Sync {
    /// List all containers (running and poweroff).
    async fn list_all(&self) -> Result<Vec<ContainerEntry>>;

    /// Start a container by name.
    async fn start(&self, name: &str) -> Result<()>;


    /// Terminate a container forcefully.
    async fn terminate(&self, name: &str) -> Result<()>;

    /// Get logs for a container.
    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>>;

    /// Get properties for a running container.
    async fn get_properties(&self, name: &str)
        -> Result<std::collections::HashMap<String, String>>;

    /// Enable automatic container start at boot.
    async fn enable(&self, name: &str) -> Result<()>;

    /// Disable automatic container start at boot.
    async fn disable(&self, name: &str) -> Result<()>;

    /// Power off a container gracefully.
    async fn poweroff(&self, name: &str) -> Result<()>;

    /// Reboot a container.
    async fn reboot(&self, name: &str) -> Result<()>;

    /// Send a signal to processes of a container.
    async fn kill(&self, name: &str, signal: &str) -> Result<()>;

    /// Check if DBus is available and being used.
    async fn is_dbus_available(&self) -> bool;

    /// Returns true if the last operation fell back to CLI (and resets the flag).
    fn did_fallback(&self) -> bool;
}
