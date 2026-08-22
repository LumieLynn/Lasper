//! Simple directory-based storage backend.

use super::super::{ManagedStorageStore, StorageBackend, StorageType};
use crate::nspawn::errors::Result;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct DirectoryBackend {
    store: ManagedStorageStore,
}

impl DirectoryBackend {
    pub fn new(store: ManagedStorageStore) -> Self {
        Self { store }
    }
}

impl Default for DirectoryBackend {
    fn default() -> Self {
        Self::new(ManagedStorageStore::default())
    }
}

#[async_trait::async_trait]
impl StorageBackend for DirectoryBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Directory
    }
    fn get_path(&self, name: &str) -> PathBuf {
        crate::paths::machine_root(name)
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        self.store.create_directory(name).await
    }

    async fn mount(&self, name: &str) -> Result<PathBuf> {
        Ok(self.get_path(name))
    }

    async fn unmount(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        self.store.remove_directory(name).await
    }

    async fn exists(&self, name: &str) -> bool {
        self.get_path(name).exists()
    }
}
