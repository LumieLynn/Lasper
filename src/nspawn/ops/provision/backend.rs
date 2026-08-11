use crate::nspawn::errors::Result;

/// Backend for provisioning operations (container creation).
///
/// Image cloning and post-deploy daemon reload use fixed CLI tools through the
/// typed system-operation boundary.
#[async_trait::async_trait]
pub trait ProvisionBackend: Send + Sync {
    async fn clone_image(&self, source: &str, dest: &str) -> Result<()>;
    async fn reload_daemon(&self) -> Result<()>;
}
