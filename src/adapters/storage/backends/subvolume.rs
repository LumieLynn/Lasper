//! Btrfs subvolume storage backend.

use super::super::{StorageBackend, StorageType};
use crate::adapters::storage::ManagedStorageStore;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use std::path::PathBuf;

pub struct SubvolumeBackend {
    store: ManagedStorageStore,
}

impl SubvolumeBackend {
    pub fn new(store: ManagedStorageStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl StorageBackend for SubvolumeBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Subvolume
    }

    fn get_path(&self, name: &str) -> PathBuf {
        crate::paths::machine_root(name)
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        self.store.create_subvolume(name).await
    }

    async fn mount(&self, name: &str) -> Result<PathBuf> {
        Ok(machine_path(&parse_machine_name(name)?))
    }

    async fn unmount(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        self.store.remove_subvolume(name).await
    }

    async fn exists(&self, name: &str) -> bool {
        let Ok(machine) = parse_machine_name(name) else {
            return false;
        };
        crate::adapters::storage::subvolume_ops::is_subvolume(&machine_path(&machine)).await
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn machine_path(machine: &MachineName) -> PathBuf {
    crate::paths::machine_root(machine.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn create_rejects_invalid_machine_name_before_external_detection() {
        let backend = SubvolumeBackend::new(ManagedStorageStore::default());
        let result = backend.create("../escape").await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn delete_rejects_invalid_machine_name_before_external_detection() {
        let backend = SubvolumeBackend::new(ManagedStorageStore::default());
        let result = backend.delete("bad/name").await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn mount_rejects_invalid_machine_name() {
        let backend = SubvolumeBackend::new(ManagedStorageStore::default());
        let result = backend.mount(".hidden").await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn exists_returns_false_for_invalid_machine_name() {
        let backend = SubvolumeBackend::new(ManagedStorageStore::default());

        assert!(!backend.exists("../escape").await);
    }
}
