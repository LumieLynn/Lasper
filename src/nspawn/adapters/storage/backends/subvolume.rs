//! Subvolume-based storage backend supporting Btrfs and ZFS.

use super::super::{StorageBackend, StorageType};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{get_filesystem_type, new_command, CommandLogged};
use std::path::{Path, PathBuf};

pub struct SubvolumeBackend;

#[derive(Debug, Clone, Copy, PartialEq)]
enum SubvolumeType {
    Btrfs,
    Zfs,
}

impl SubvolumeBackend {
    async fn detect_type(&self) -> Result<SubvolumeType> {
        let machines_dir = Path::new("/var/lib/machines");
        let fs_type = get_filesystem_type(machines_dir).await?;

        match fs_type.as_str() {
            "btrfs" => Ok(SubvolumeType::Btrfs),
            "zfs" => Ok(SubvolumeType::Zfs),
            _ => Err(NspawnError::Generic(format!(
                "Path /var/lib/machines is on {} which does not support subvolumes in this context",
                fs_type
            ))),
        }
    }

    /// Gets the ZFS dataset name for a given path.
    async fn get_zfs_dataset(&self, path: &Path) -> Result<String> {
        let out = crate::nspawn::sys::new_command("zfs")
            .args(["list", "-H", "-o", "name", &path.to_string_lossy()])
            .logged_output("zfs")
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("zfs"), e))?;

        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(NspawnError::cmd_failed(
                "zfs list dataset",
                format!("zfs list -H -o name {}", path.display()),
                &out,
            ))
        }
    }
}

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
        match self.detect_type().await? {
            SubvolumeType::Btrfs => {
                let out = new_command("btrfs")
                    .args(["subvolume", "create", &path.to_string_lossy()])
                    .logged_output("btrfs")
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;

                if !out.status.success() {
                    return Err(NspawnError::cmd_failed(
                        "btrfs subvolume create",
                        format!("btrfs subvolume create {}", path.display()),
                        &out,
                    ));
                }
            }
            SubvolumeType::Zfs => {
                let machines_dir = Path::new("/var/lib/machines");
                let parent_dataset = self.get_zfs_dataset(machines_dir).await?;
                let dataset_name = format!("{}/{}", parent_dataset, name);

                let out = new_command("zfs")
                    .args(["create", &dataset_name])
                    .logged_output("zfs")
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("zfs"), e))?;

                if !out.status.success() {
                    return Err(NspawnError::cmd_failed(
                        "zfs create dataset",
                        format!("zfs create {}", dataset_name),
                        &out,
                    ));
                }
            }
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
        match self.detect_type().await? {
            SubvolumeType::Btrfs => {
                let out = new_command("btrfs")
                    .args(["subvolume", "delete", &path.to_string_lossy()])
                    .logged_output("btrfs")
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;

                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if err.contains("no such file or directory") || err.contains("not a subvolume")
                    {
                        log::warn!(
                            "Btrfs subvolume already missing for deletion: {}",
                            path.display()
                        );
                    } else {
                        return Err(NspawnError::cmd_failed(
                            "btrfs subvolume delete",
                            format!("btrfs subvolume delete {}", path.display()),
                            &out,
                        ));
                    }
                }
            }
            SubvolumeType::Zfs => {
                let machines_dir = Path::new("/var/lib/machines");
                let parent_dataset = match self.get_zfs_dataset(machines_dir).await {
                    Ok(ds) => ds,
                    Err(_) => return Ok(()), // Machines dir not a ZFS dataset, nothing to destroy
                };
                let dataset_name = format!("{}/{}", parent_dataset, name);

                let out = new_command("zfs")
                    .args(["destroy", &dataset_name])
                    .logged_output("zfs")
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("zfs"), e))?;

                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if err.contains("dataset does not exist") {
                        log::warn!("ZFS dataset already missing for deletion: {}", dataset_name);
                    } else {
                        return Err(NspawnError::cmd_failed(
                            "zfs destroy dataset",
                            format!("zfs destroy {}", dataset_name),
                            &out,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn exists(&self, name: &str) -> bool {
        tokio::fs::try_exists(self.get_path(name))
            .await
            .unwrap_or(false)
    }
}
