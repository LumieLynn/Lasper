//! Abstraction for different container storage backends.

use super::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StorageType {
    Directory,
    Subvolume,
    Raw,
}

impl StorageType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Directory => "Directory",
            Self::Subvolume => "Btrfs Subvolume",
            Self::Raw => "Raw Sparse File",
        }
    }

    pub fn get_path(&self, name: &str) -> PathBuf {
        match self {
            Self::Directory | Self::Subvolume => {
                PathBuf::from(format!("/var/lib/machines/{}", name))
            }
            Self::Raw => PathBuf::from(format!("/var/lib/machines/{}.raw", name)),
        }
    }
}

/// Information about the available storage backends on the host.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageInfo {
    pub types: Vec<(StorageType, bool)>, // (type, supported)
}

pub fn detect_available_storage_types() -> StorageInfo {
    let machines_dir = Path::new("/var/lib/machines");
    let mut types = vec![
        (StorageType::Directory, true),
        (StorageType::Raw, true),
        (StorageType::Subvolume, false),
    ];

    if let Ok(fs_type) = get_filesystem_type(machines_dir) {
        if fs_type == "btrfs" {
            for t in &mut types {
                if t.0 == StorageType::Subvolume {
                    t.1 = true;
                }
            }
        }
    }

    StorageInfo { types }
}

fn get_filesystem_type(path: &Path) -> Result<String> {
    // We can use 'stat -f -c %T <path>' to get the filesystem type name
    let out = std::process::Command::new("stat")
        .args(["-f", "-c", "%T", &path.to_string_lossy()])
        .output()
        .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(NspawnError::CommandFailed(
            "stat".into(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Trait for managing container rootfs storage.
#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    fn get_type(&self) -> StorageType;
    fn get_path(&self, name: &str) -> PathBuf;
    async fn create(&self, name: &str) -> Result<PathBuf>;

    /// Mount the storage and return the path to the rootfs.
    /// For directory-based storage, this simply returns the path.
    /// For raw files, it sets up a loop device and mounts it.
    async fn mount(&self, name: &str) -> Result<PathBuf>;

    /// Unmount the storage.
    async fn unmount(&self, _name: &str) -> Result<()>;

    #[allow(dead_code)]
    async fn delete(&self, name: &str) -> Result<()>;
    #[allow(dead_code)]
    async fn exists(&self, name: &str) -> bool;
}

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

pub struct SubvolumeBackend;

#[async_trait::async_trait]
impl StorageBackend for SubvolumeBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Subvolume
    }
    fn get_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/var/lib/machines/{}", name))
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        let path = self.get_path(name);
        let out = Command::new("btrfs")
            .args(["subvolume", "create", &path.to_string_lossy()])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;
        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                "btrfs subvolume create".into(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
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
        let out = Command::new("btrfs")
            .args(["subvolume", "delete", &path.to_string_lossy()])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("no such file or directory") || err.contains("not a subvolume") {
                log::warn!("Btrfs subvolume already missing for deletion: {}", path.display());
            } else {
                return Err(NspawnError::CommandFailed(
                    "btrfs subvolume delete".into(),
                    err.to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn exists(&self, name: &str) -> bool {
        self.get_path(name).exists()
    }
}

pub struct RawBackend {
    pub config: super::models::RawStorageConfig,
}

#[async_trait::async_trait]
impl StorageBackend for RawBackend {
    fn get_type(&self) -> StorageType {
        StorageType::Raw
    }
    fn get_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/var/lib/machines/{}.raw", name))
    }

    async fn create(&self, name: &str) -> Result<PathBuf> {
        let path = self.get_path(name);
        // Create a sparse file of the specified size
        let out = Command::new("truncate")
            .args(["-s", &self.config.size, &path.to_string_lossy()])
            .output()
            .await
            .map_err(|e| NspawnError::Io(path.clone(), e))?;
        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                "truncate".into(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }

        if self.config.use_partition_table {
            log::warn!("Custom partition table requested but not yet implemented. Formatting whole image instead.");
        }

        // Format with the specified filesystem
        let mkfs_prog = format!("mkfs.{}", self.config.fs_type);
        let out = Command::new(&mkfs_prog)
            .args(["-F", &path.to_string_lossy()])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from(&mkfs_prog), e))?;
        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                mkfs_prog,
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        Ok(path)
    }

    async fn mount(&self, name: &str) -> Result<PathBuf> {
        let img_path = self.get_path(name);
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));

        // Create mount point
        tokio::fs::create_dir_all(&mount_point)
            .await
            .map_err(|e| NspawnError::Io(mount_point.clone(), e))?;

        // Find/Setup loop device and mount in one go using 'mount -o loop'
        let out = Command::new("mount")
            .args([
                "-o",
                "loop",
                &img_path.to_string_lossy(),
                &mount_point.to_string_lossy(),
            ])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("mount"), e))?;

        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                "mount".into(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }

        Ok(mount_point)
    }

    async fn unmount(&self, name: &str) -> Result<()> {
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));

        let out = Command::new("umount")
            .arg(&mount_point)
            .output()
            .await
            .map_err(|e| NspawnError::Io(mount_point.clone(), e))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("not mounted") && !err.contains("no such file or directory") {
                return Err(NspawnError::CommandFailed("umount".into(), err.to_string()));
            }
        }

        // Clean up mount point
        if let Err(e) = tokio::fs::remove_dir(&mount_point).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Failed to remove mount point {}: {}", mount_point.display(), e);
            }
        }

        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let path = self.get_path(name);
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::warn!("Image file already missing for deletion: {}", path.display());
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
