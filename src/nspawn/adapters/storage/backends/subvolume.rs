//! Subvolume-based storage backend supporting Btrfs and ZFS.

use super::super::{StorageBackend, StorageType};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
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

    async fn get_zfs_dataset(&self, path: &Path, cmd_runner: &dyn CommandRunner) -> Result<String> {
        let out = cmd_runner
            .run(
                "zfs",
                vec![
                    "list".into(),
                    "-H".into(),
                    "-o".into(),
                    "name".into(),
                    path.to_string_lossy().to_string(),
                ],
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
        let machine = parse_machine_name(name)?;
        let path = machine_path(&machine);
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
                let dataset_name = zfs_child_dataset(&parent_dataset, &machine)?;
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
        Ok(machine_path(&parse_machine_name(name)?))
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
        let machine = parse_machine_name(name)?;
        let path = machine_path(&machine);
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
                    if err.contains("no such file or directory") || err.contains("not a subvolume")
                    {
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
                let dataset_name = zfs_child_dataset(&parent_dataset, &machine)?;
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
        let Ok(machine) = parse_machine_name(name) else {
            return false;
        };
        tokio::fs::try_exists(machine_path(&machine))
            .await
            .unwrap_or(false)
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn machine_path(machine: &MachineName) -> PathBuf {
    crate::paths::machine_root(machine.as_str())
}

fn zfs_child_dataset(parent_dataset: &str, machine: &MachineName) -> Result<String> {
    let parent_dataset = parent_dataset.trim();
    if parent_dataset.is_empty() || parent_dataset.chars().any(char::is_control) {
        return Err(NspawnError::Validation(
            "Invalid parent ZFS dataset name".into(),
        ));
    }
    Ok(format!("{}/{}", parent_dataset, machine.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::ops::PermissionLevel;
    use crate::nspawn::sys::command::MockCommandRunner;

    #[tokio::test]
    async fn create_rejects_invalid_machine_name_before_external_detection() {
        let backend = SubvolumeBackend;
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();
        let io = ElevatedIo::new(PermissionLevel::Root);

        let result = backend.create("../escape", &runner, &io).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn delete_rejects_invalid_machine_name_before_external_detection() {
        let backend = SubvolumeBackend;
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();
        let io = ElevatedIo::new(PermissionLevel::Root);

        let result = backend.delete("bad/name", &runner, &io).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn mount_rejects_invalid_machine_name() {
        let backend = SubvolumeBackend;
        let runner = MockCommandRunner::new();
        let io = ElevatedIo::new(PermissionLevel::Root);

        let result = backend.mount(".hidden", &runner, &io).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn exists_returns_false_for_invalid_machine_name() {
        let backend = SubvolumeBackend;

        assert!(!backend.exists("../escape").await);
    }

    #[test]
    fn zfs_dataset_child_uses_validated_machine_name() {
        let machine = parse_machine_name("valid_1").unwrap();

        assert_eq!(
            zfs_child_dataset("tank/machines", &machine).unwrap(),
            "tank/machines/valid_1"
        );
        assert!(zfs_child_dataset("", &machine).is_err());
        assert!(zfs_child_dataset("tank\nmachines", &machine).is_err());
    }
}
