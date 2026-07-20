//! Disk image storage backend.

pub mod create;
pub mod mount;

use super::super::{
    ImageMountSource, ManagedImageKind, ManagedStorageStore, StorageBackend, StorageType,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::DiskImageConfig;
use crate::nspawn::sys::{CommandRunner, ElevatedIo};
use std::path::PathBuf;

pub struct DiskImageBackend {
    pub config: DiskImageConfig,
    store: ManagedStorageStore,
    location: DiskImageLocation,
}

#[derive(Clone, Debug)]
enum DiskImageLocation {
    Managed(ManagedImageKind),
    External(PathBuf),
}

impl DiskImageBackend {
    pub fn new(config: DiskImageConfig, store: ManagedStorageStore) -> Self {
        Self {
            config,
            store,
            location: DiskImageLocation::Managed(ManagedImageKind::Raw),
        }
    }

    pub(crate) fn existing_managed(
        config: DiskImageConfig,
        kind: ManagedImageKind,
        store: ManagedStorageStore,
    ) -> Self {
        Self {
            config,
            store,
            location: DiskImageLocation::Managed(kind),
        }
    }

    pub(crate) fn external(
        config: DiskImageConfig,
        path: PathBuf,
        store: ManagedStorageStore,
    ) -> Self {
        Self {
            config,
            store,
            location: DiskImageLocation::External(path),
        }
    }

    fn managed_kind(&self) -> Option<ManagedImageKind> {
        match self.location {
            DiskImageLocation::Managed(kind) => Some(kind),
            DiskImageLocation::External(_) => None,
        }
    }

    fn mount_source(&self, name: &str) -> Result<ImageMountSource> {
        match &self.location {
            DiskImageLocation::Managed(kind) => Ok(ImageMountSource::Managed(*kind)),
            DiskImageLocation::External(path) if path == &PathBuf::from("/dev").join(name) => {
                Ok(ImageMountSource::BlockDevice)
            }
            DiskImageLocation::External(path) => Err(NspawnError::Validation(format!(
                "Refusing untyped external image path: {}",
                path.display()
            ))),
        }
    }
}

#[async_trait::async_trait]
impl StorageBackend for DiskImageBackend {
    fn get_type(&self) -> StorageType {
        StorageType::DiskImage
    }

    fn get_path(&self, name: &str) -> PathBuf {
        match &self.location {
            DiskImageLocation::Managed(ManagedImageKind::Raw) => {
                crate::paths::machine_raw_image(name)
            }
            DiskImageLocation::Managed(ManagedImageKind::LegacyImg) => {
                crate::paths::machine_image(name, "img")
            }
            DiskImageLocation::External(path) => path.clone(),
        }
    }

    async fn create(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<PathBuf> {
        self.create_impl(name).await
    }

    async fn mount(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<PathBuf> {
        self.mount_impl(name).await
    }

    async fn unmount(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<()> {
        self.unmount_impl(name).await
    }

    async fn delete(
        &self,
        name: &str,
        _cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<()> {
        let Some(kind) = self.managed_kind() else {
            return Err(NspawnError::Validation(format!(
                "Refusing to delete externally managed image path: {}",
                self.get_path(name).display()
            )));
        };
        self.store.remove_image(name, kind).await
    }

    async fn exists(&self, name: &str) -> bool {
        tokio::fs::try_exists(self.get_path(name))
            .await
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{DiskImageFilesystem, DiskImageSource};

    #[test]
    fn new_imports_publish_to_canonical_raw_path() {
        let backend = DiskImageBackend::new(
            DiskImageConfig {
                source: DiskImageSource::ImportExisting {
                    path: "/tmp/source.img".into(),
                },
                use_partition_table: false,
                root_partition: None,
            },
            ManagedStorageStore::default(),
        );

        assert_eq!(
            backend.get_path("test"),
            crate::paths::machine_raw_image("test")
        );
    }

    #[test]
    fn existing_legacy_img_keeps_its_detected_path() {
        let backend = DiskImageBackend::existing_managed(
            DiskImageConfig {
                source: DiskImageSource::ImportExisting {
                    path: "/var/lib/machines/test.img".into(),
                },
                use_partition_table: false,
                root_partition: None,
            },
            ManagedImageKind::LegacyImg,
            ManagedStorageStore::default(),
        );

        assert_eq!(
            backend.get_path("test"),
            crate::paths::machine_image("test", "img")
        );
    }

    #[test]
    fn default_disk_image_config_uses_partition_table() {
        assert_eq!(
            DiskImageConfig::default().source,
            DiskImageSource::CreateNew {
                size: "10G".into(),
                fs_type: DiskImageFilesystem::Ext4,
            }
        );
        assert!(DiskImageConfig::default().use_partition_table);
    }
}
