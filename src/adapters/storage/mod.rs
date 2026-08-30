//! Storage backend management for systemd-nspawn containers.

pub mod backends;
pub mod detect;
mod fs_type;
pub(crate) mod image_ops;
pub mod store;
pub(crate) mod subvolume_ops;

use crate::adapters::error::Result;
use crate::domain::storage::{DiskImageConfig, DiskImageSource};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use backends::directory::DirectoryBackend;
pub use backends::image::DiskImageBackend;
pub use backends::subvolume::SubvolumeBackend;
pub(crate) use fs_type::get_filesystem_type;
pub use store::{ImageMountSource, ManagedImageKind, ManagedStorageStore};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StorageType {
    Directory,
    Subvolume,
    DiskImage,
}

impl StorageType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Directory => "Directory",
            Self::Subvolume => "Btrfs Subvolume",
            Self::DiskImage => "Disk Image (Raw/Block)",
        }
    }
}

/// Information about the available storage backends on the host.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageInfo {
    pub types: Vec<(StorageType, bool)>, // (type, supported)
}

/// Trait for managing container rootfs storage.
#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    fn get_type(&self) -> StorageType;
    fn get_path(&self, name: &str) -> PathBuf;
    async fn create(&self, name: &str) -> Result<PathBuf>;

    /// Mount the storage and return the path to the rootfs.
    async fn mount(&self, name: &str) -> Result<PathBuf>;

    /// Unmount the storage.
    async fn unmount(&self, name: &str) -> Result<()>;

    async fn delete(&self, name: &str) -> Result<()>;
    #[allow(dead_code)]
    async fn exists(&self, name: &str) -> bool;
}

// Helper to eliminate verbose explicit trait object casting and aid type inference
#[allow(dead_code)]
#[inline]
fn into_backend<T: StorageBackend + 'static>(backend: T) -> Box<dyn StorageBackend> {
    Box::new(backend)
}

#[allow(dead_code)] // mount/unmount paths removed; kept for future storage-aware operations
/// Factory function to get the appropriate storage backend for an existing machine.
pub async fn get_storage_backend_for(
    name: &str,
    managed_storage: ManagedStorageStore,
) -> Box<dyn StorageBackend> {
    let base = crate::paths::machine_root(name);

    // 1. Check for raw disk image extensions (only raw is supported by systemd-nspawn)
    let extensions = [
        ("raw", ManagedImageKind::Raw),
        ("img", ManagedImageKind::LegacyImg),
    ];
    for (ext, kind) in extensions {
        let path = base.with_extension(ext);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return into_backend(DiskImageBackend::existing_managed(
                DiskImageConfig {
                    source: DiskImageSource::ImportExisting {
                        path: path.to_string_lossy().to_string(),
                    },
                    use_partition_table: false,
                    root_partition: None,
                },
                kind,
                managed_storage.clone(),
            ));
        }
    }

    // 2. Check if a block device exists with this name (e.g. /dev/name)
    let block_dev = PathBuf::from(format!("/dev/{}", name));
    if let Ok(meta) = tokio::fs::metadata(&block_dev).await {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_block_device() {
            return into_backend(DiskImageBackend::external(
                DiskImageConfig {
                    source: DiskImageSource::ImportExisting {
                        path: block_dev.to_string_lossy().to_string(),
                    },
                    use_partition_table: false,
                    root_partition: None,
                },
                block_dev,
                managed_storage.clone(),
            ));
        }
    }

    // 3. Check if it's a Btrfs subvolume
    if detect::is_subvolume(&base).await {
        return into_backend(SubvolumeBackend::new(managed_storage.clone()));
    }

    // 4. Default to DirectoryBackend
    into_backend(DirectoryBackend::new(managed_storage))
}
