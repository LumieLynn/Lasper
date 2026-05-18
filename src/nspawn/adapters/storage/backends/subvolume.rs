//! Subvolume-based storage backend supporting Btrfs and ZFS.

use super::super::{StorageBackend, StorageType};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{get_filesystem_type, log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};

pub struct SubvolumeBackend;

#[derive(Debug, Clone, Copy, PartialEq)]
enum SubvolumeType {
    Btrfs,
    Zfs,
}

impl SubvolumeBackend {
    async fn detect_type(&self) -> Result<SubvolumeType> {
        let machines_dir = crate::paths::machines_dir();
        let fs_type = get_filesystem_type(&machines_dir).await?;
        match fs_type.as_str() {
            "btrfs" => Ok(SubvolumeType::Btrfs),
            "zfs" => Ok(SubvolumeType::Zfs),
            _ => Err(NspawnError::Generic(format!(
                "/var/lib/machines is on {} which does not support subvolumes",
                fs_type
            ))),
        }
    }

    async fn get_zfs_dataset(
        &self,
        path: &Path,
        cmd_runner: &dyn CommandRunner,
    ) -> Result<String> {
        let out = cmd_runner
            .run(
                "zfs",
                vec!["list".into(), "-H".into(), "-o".into(), "name".into(), path.to_string_lossy().to_string()],
            )
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
        crate::paths::machine_root(name)
    }

    async fn create(
        &self,
        name: &str,
        cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<PathBuf> {
        let path = self.get_path(name);
        match self.detect_type().await? {
            SubvolumeType::Btrfs => {
                let out = cmd_runner
                    .run(
                        "btrfs",
                        vec![
                            "subvolume".into(),
                            "create".into(),
                            path.to_string_lossy().to_string(),
                        ],
                    )
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;
                log_output("btrfs", &out);
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed(
                        "btrfs subvolume create",
                        format!("btrfs subvolume create {}", path.display()),
                        &out,
                    ));
                }
            }
            SubvolumeType::Zfs => {
                let machines_dir = crate::paths::machines_dir();
                let parent_dataset = self.get_zfs_dataset(&machines_dir, cmd_runner).await?;
                let dataset_name = format!("{}/{}", parent_dataset, name);
                let out = cmd_runner
                    .run("zfs", vec!["create".into(), dataset_name.clone()])
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("zfs"), e))?;
                log_output("zfs", &out);
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
        cmd_runner: &dyn CommandRunner,
        _io: &ElevatedIo,
    ) -> Result<()> {
        let path = self.get_path(name);
        match self.detect_type().await? {
            SubvolumeType::Btrfs => {
                let out = cmd_runner
                    .run(
                        "btrfs",
                        vec![
                            "subvolume".into(),
                            "delete".into(),
                            path.to_string_lossy().to_string(),
                        ],
                    )
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("btrfs"), e))?;
                log_output("btrfs", &out);
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if err.contains("no such file or directory") || err.contains("not a subvolume") {
                        log::warn!("Btrfs subvolume already missing: {}", path.display());
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
                let machines_dir = crate::paths::machines_dir();
                let parent_dataset = match self.get_zfs_dataset(&machines_dir, cmd_runner).await {
                    Ok(ds) => ds,
                    Err(_) => return Ok(()),
                };
                let dataset_name = format!("{}/{}", parent_dataset, name);
                let out = cmd_runner
                    .run("zfs", vec!["destroy".into(), dataset_name.clone()])
                    .await
                    .map_err(|e| NspawnError::Io(PathBuf::from("zfs"), e))?;
                log_output("zfs", &out);
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if err.contains("dataset does not exist") {
                        log::warn!("ZFS dataset already missing: {}", dataset_name);
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
