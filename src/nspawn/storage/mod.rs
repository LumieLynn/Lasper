//! Storage backend management for systemd-nspawn containers.

pub mod backends;
pub mod detect;

use crate::nspawn::errors::Result;
use crate::nspawn::models::{DiskImageConfig, DiskImageSource};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use backends::directory::DirectoryBackend;
pub use backends::image::DiskImageBackend;
pub use backends::subvolume::SubvolumeBackend;

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
            Self::Subvolume => "Subvolume (Btrfs/Generic)",
            Self::DiskImage => "Disk Image (Raw/Block)",
        }
    }

    pub fn get_path(&self, name: &str) -> PathBuf {
        match self {
            Self::Directory | Self::Subvolume => {
                PathBuf::from(format!("/var/lib/machines/{}", name))
            }
            Self::DiskImage => {
                // Only raw disk images are supported by systemd-nspawn
                let base = PathBuf::from("/var/lib/machines").join(name);
                for ext in ["raw", "img"] {
                    let p = base.with_extension(ext);
                    if p.exists() {
                        return p;
                    }
                }
                base.with_extension("raw") // Default to .raw for new
            }
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
    async fn exists(&self, name: &str) -> bool;
}

/// Factory function to get the appropriate storage backend for an existing machine.
pub fn get_storage_backend_for(name: &str) -> Box<dyn StorageBackend> {
    let base = PathBuf::from("/var/lib/machines").join(name);

    // 1. Check for raw disk image extensions (only raw is supported by systemd-nspawn)
    let extensions = ["raw", "img", "iso"];
    for ext in extensions {
        let path = base.with_extension(ext);
        if path.exists() {
            return Box::new(DiskImageBackend {
                config: DiskImageConfig {
                    source: DiskImageSource::ImportExisting {
                        path: path.to_string_lossy().to_string(),
                    },
                    use_partition_table: false,
                },
            });
        }
    }

    // 2. Check if a block device exists with this name (e.g. /dev/name)
    let block_dev = PathBuf::from(format!("/dev/{}", name));
    if block_dev.exists() {
        if let Ok(meta) = std::fs::metadata(&block_dev) {
            use std::os::unix::fs::FileTypeExt;
            if meta.file_type().is_block_device() {
                return Box::new(DiskImageBackend {
                    config: DiskImageConfig {
                        source: DiskImageSource::ImportExisting {
                            path: block_dev.to_string_lossy().to_string(),
                        },
                        use_partition_table: false,
                    },
                });
            }
        }
    }

    // 3. Check if it's a subvolume (Btrfs subvolume or ZFS dataset)
    if crate::nspawn::storage::detect::is_subvolume(&base) {
        return Box::new(SubvolumeBackend);
    }

    // 4. Default to DirectoryBackend
    Box::new(DirectoryBackend)
}
