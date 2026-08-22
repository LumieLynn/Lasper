use super::DiskImageBackend;
use crate::nspawn::errors::Result;
use std::path::PathBuf;

impl DiskImageBackend {
    pub(super) async fn mount_impl(&self, name: &str) -> Result<PathBuf> {
        if let Some(partition) = self.config.root_partition {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Selected p{} as the managed image root partition; its GPT type may be normalized on the managed copy.",
                name,
                partition.number()
            );
        }
        self.store
            .mount_image(name, self.mount_source(name)?, self.config.root_partition)
            .await
    }

    pub(super) async fn unmount_impl(&self, name: &str) -> Result<()> {
        self.store
            .unmount_image(name, self.mount_source(name)?)
            .await
    }
}
