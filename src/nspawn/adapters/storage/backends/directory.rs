//! Simple directory-based storage backend.

use super::super::{StorageBackend, StorageType};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{CommandRunner, ElevatedIo};
use std::path::PathBuf;

pub struct DirectoryBackend;

#[async_trait::async_trait]
impl StorageBackend for DirectoryBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Directory
    }
    fn get_path(&self, name: &str) -> PathBuf {
        crate::paths::machine_root(name)
    }

    async fn create(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        io: &ElevatedIo,
    ) -> Result<PathBuf> {
        let path = self.get_path(name);
        io.create_dir_all(&path).await?;
        Ok(path)
    }

    async fn mount(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<PathBuf> {
        Ok(self.get_path(name))
    }

    async fn unmount(
        &self,
        _name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<()> {
        Ok(())
    }

    async fn delete(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        io: &ElevatedIo,
    ) -> Result<()> {
        let path = self.get_path(name);
        if let Err(e) = io.remove_dir_all(&path).await {
            if matches!(
                e,
                NspawnError::Io(_, ref io_err)
                    if io_err.kind() == std::io::ErrorKind::NotFound
            ) {
                log::warn!("Directory already missing for deletion: {}", path.display());
            } else {
                return Err(e);
            }
        }
        Ok(())
    }

    async fn exists(&self, name: &str) -> bool {
        self.get_path(name).exists()
    }
}
