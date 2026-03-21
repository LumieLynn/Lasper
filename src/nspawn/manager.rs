use super::errors::Result;
use super::machinectl::ContainerEntry;
use super::models::ContainerConfig;
use async_trait::async_trait;

#[async_trait]
#[allow(dead_code)]
pub trait NspawnManager: Send + Sync {
    /// List all containers (running and stopped).
    async fn list_all(&self) -> Result<Vec<ContainerEntry>>;

    /// Start a container by name.
    async fn start(&self, name: &str) -> Result<()>;

    /// Stop a container by name.
    async fn stop(&self, name: &str) -> Result<()>;

    /// Terminate a container forcefully.
    async fn terminate(&self, name: &str) -> Result<()>;

    /// Get logs for a container.
    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>>;

    /// Get properties for a running container.
    async fn get_properties(&self, name: &str)
        -> Result<std::collections::HashMap<String, String>>;

    /// Create a new container with the given configuration and storage backend.
    async fn create(
        &self,
        cfg: &ContainerConfig,
        storage: &dyn super::storage::StorageBackend,
    ) -> Result<()>;
}
