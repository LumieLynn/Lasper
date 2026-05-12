//! Disk image storage backend.

pub mod create;
pub mod mount;
pub mod utils;

use super::super::{StorageBackend, StorageType};
use crate::nspawn::errors::Result;
use crate::nspawn::models::DiskImageConfig;
use std::path::PathBuf;

pub struct DiskImageBackend {
    pub config: DiskImageConfig,
}

#[async_trait::async_trait]
impl StorageBackend for DiskImageBackend {
    fn get_type(&self) -> StorageType {
        StorageType::DiskImage
    }

    fn get_path(&self, name: &str) -> PathBuf {
        use crate::nspawn::models::DiskImageSource;
        match &self.config.source {
            DiskImageSource::ImportExisting { path } => {
                let src_path = PathBuf::from(path);
                let ext = src_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("raw");
                crate::paths::machine_image(name, ext)
            }
            DiskImageSource::CreateNew { .. } => crate::paths::machine_raw_image(name),
        }
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        self.create_impl(name).await
    }

    async fn mount(&self, name: &str) -> Result<PathBuf> {
        self.mount_impl(name).await
    }

    async fn unmount(&self, name: &str) -> Result<()> {
        self.unmount_impl(name).await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let path = self.get_path(name);
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::warn!(
                    "Image file already missing for deletion: {}",
                    path.display()
                );
            } else {
                return Err(crate::nspawn::errors::NspawnError::Io(path, e));
            }
        }
        Ok(())
    }

    async fn exists(&self, name: &str) -> bool {
        let base = crate::paths::machine_root(name);
        for ext in ["raw", "img"] {
            if tokio::fs::try_exists(base.with_extension(ext))
                .await
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }
}
