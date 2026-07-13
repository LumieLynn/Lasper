use crate::nspawn::errors::Result;

/// Backend for provisioning operations (container creation).
///
/// Provisioning always uses CLI tools (`machinectl`, `systemctl`) —
/// there is no DBus alternative for image cloning or the post-deploy
/// daemon-reload that follows config writes.
#[async_trait::async_trait]
pub trait ProvisionBackend: Send + Sync {
    async fn clone_image(&self, source: &str, dest: &str) -> Result<()>;
    async fn reload_daemon(&self) -> Result<()>;
}
