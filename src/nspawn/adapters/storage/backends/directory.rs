//! Simple directory-based storage backend.

use std::path::PathBuf;
use crate::nspawn::errors::{NspawnError, Result};
use super::super::{StorageBackend, StorageType};

pub struct DirectoryBackend;

#[async_trait::async_trait]
impl StorageBackend for DirectoryBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Directory
    }
    fn get_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/var/lib/machines/{}", name))
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        let path = self.get_path(name);
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| NspawnError::Io(path.clone(), e))?;
        Ok(path)
    }

    async fn mount(&self, name: &str) -> Result<PathBuf> {
        Ok(self.get_path(name))
    }

    async fn unmount(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let path = self.get_path(name);
        if let Err(e) = tokio::fs::remove_dir_all(&path).await {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::warn!("Directory already missing for deletion: {}", path.display());
            } else {
                return Err(NspawnError::Io(path, e));
            }
        }
        Ok(())
    }

    async fn exists(&self, name: &str) -> bool {
        self.get_path(name).exists()
    }
}
