use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Typed access to Lasper-managed storage paths.
#[derive(Clone)]
pub struct ManagedStorageStore {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl ManagedStorageStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub async fn create_directory(&self, name: &str) -> Result<PathBuf> {
        let result = self
            .execute(ManagedStorageOperation::CreateDirectory(
                CreateManagedDirectory {
                    machine: parse_machine_name(name)?,
                },
            ))
            .await?;
        result.path.ok_or_else(|| {
            NspawnError::Runtime("managed storage operation returned no path".into())
        })
    }

    pub async fn remove_directory(&self, name: &str) -> Result<()> {
        self.execute(ManagedStorageOperation::RemoveDirectory(
            RemoveManagedDirectory {
                machine: parse_machine_name(name)?,
            },
        ))
        .await?;
        Ok(())
    }

    pub async fn reserve_raw_image(&self, name: &str) -> Result<PathBuf> {
        let result = self
            .execute(ManagedStorageOperation::ReserveRawImage(
                ReserveManagedRawImage {
                    machine: parse_machine_name(name)?,
                },
            ))
            .await?;
        result.path.ok_or_else(|| {
            NspawnError::Runtime("managed storage operation returned no path".into())
        })
    }

    pub async fn remove_image(&self, name: &str, kind: ManagedImageKind) -> Result<()> {
        self.execute(ManagedStorageOperation::RemoveImage(RemoveManagedImage {
            machine: parse_machine_name(name)?,
            kind,
        }))
        .await?;
        Ok(())
    }

    async fn execute(&self, operation: ManagedStorageOperation) -> Result<ManagedStorageResult> {
        if let Some(daemon) = &self.daemon {
            daemon
                .managed_storage(operation)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            execute_managed_storage_operation(operation).await
        }
    }
}

impl std::fmt::Debug for ManagedStorageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedStorageStore")
            .field("daemon", &self.daemon)
            .finish()
    }
}

impl Default for ManagedStorageStore {
    fn default() -> Self {
        Self::new(None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum ManagedStorageOperation {
    CreateDirectory(CreateManagedDirectory),
    RemoveDirectory(RemoveManagedDirectory),
    ReserveRawImage(ReserveManagedRawImage),
    RemoveImage(RemoveManagedImage),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateManagedDirectory {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveManagedDirectory {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReserveManagedRawImage {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveManagedImage {
    machine: MachineName,
    kind: ManagedImageKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedImageKind {
    Raw,
    LegacyImg,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedStorageResult {
    path: Option<PathBuf>,
}

pub(crate) async fn execute_managed_storage_operation(
    operation: ManagedStorageOperation,
) -> Result<ManagedStorageResult> {
    match operation {
        ManagedStorageOperation::CreateDirectory(request) => {
            let path = directory_path(&request.machine);
            let conflicts = [
                image_path(&request.machine, ManagedImageKind::Raw),
                image_path(&request.machine, ManagedImageKind::LegacyImg),
            ];
            create_directory_at(&path, &conflicts).await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::RemoveDirectory(request) => {
            remove_directory_at(&directory_path(&request.machine)).await?;
            Ok(ManagedStorageResult::default())
        }
        ManagedStorageOperation::ReserveRawImage(request) => {
            let path = image_path(&request.machine, ManagedImageKind::Raw);
            let conflicts = [
                directory_path(&request.machine),
                image_path(&request.machine, ManagedImageKind::LegacyImg),
            ];
            reserve_raw_image_at(&path, &conflicts).await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::RemoveImage(request) => {
            remove_image_at(&image_path(&request.machine, request.kind)).await?;
            Ok(ManagedStorageResult::default())
        }
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn directory_path(machine: &MachineName) -> PathBuf {
    crate::paths::machine_root(machine.as_str())
}

fn image_path(machine: &MachineName, kind: ManagedImageKind) -> PathBuf {
    match kind {
        ManagedImageKind::Raw => crate::paths::machine_raw_image(machine.as_str()),
        ManagedImageKind::LegacyImg => crate::paths::machine_image(machine.as_str(), "img"),
    }
}

async fn create_directory_at(path: &Path, conflicts: &[PathBuf]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    }

    reject_existing_paths(conflicts).await?;

    match tokio::fs::create_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(NspawnError::Validation(format!(
                "Managed directory already exists: {}",
                path.display()
            )))
        }
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_directory_at(path: &Path) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
    };

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Refusing to remove non-directory managed storage path: {}",
            path.display()
        )));
    }

    tokio::fs::remove_dir_all(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))
}

async fn reserve_raw_image_at(path: &Path, conflicts: &[PathBuf]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    }

    reject_existing_paths(conflicts).await?;

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file
            .sync_all()
            .map_err(|error| NspawnError::Io(path.to_path_buf(), error)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(NspawnError::Validation(format!(
                "Managed storage already exists: {}",
                path.display()
            )))
        }
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn reject_existing_paths(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => {
                return Err(NspawnError::Validation(format!(
                    "Managed storage already exists: {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(NspawnError::Io(path.clone(), error)),
        }
    }
    Ok(())
}

async fn remove_image_at(path: &Path) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
    };

    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NspawnError::Validation(format!(
            "Refusing to remove non-file managed image path: {}",
            path.display()
        )));
    }

    tokio::fs::remove_file(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_deserialization_rejects_invalid_machine_name() {
        let json = r#"{
            "operation": "create_directory",
            "params": {"machine": "../escape"}
        }"#;
        assert!(serde_json::from_str::<ManagedStorageOperation>(json).is_err());
    }

    #[tokio::test]
    async fn create_directory_fails_closed_when_target_exists() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("machine");
        tokio::fs::create_dir(&target).await.unwrap();

        let result = create_directory_at(&target, &[]).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_and_remove_directory_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("machine");

        create_directory_at(&target, &[]).await.unwrap();
        assert!(target.is_dir());

        remove_directory_at(&target).await.unwrap();
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn remove_directory_rejects_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let link = directory.path().join("link");
        tokio::fs::create_dir(&real).await.unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let result = remove_directory_at(&link).await;

        assert!(result.is_err());
        assert!(real.exists());
        assert!(link.exists());
    }

    #[tokio::test]
    async fn create_directory_rejects_raw_and_legacy_image_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("machine");
        let raw = directory.path().join("machine.raw");
        let legacy = directory.path().join("machine.img");
        tokio::fs::write(&raw, b"raw").await.unwrap();

        let result = create_directory_at(&target, &[raw.clone(), legacy.clone()]).await;
        assert!(result.is_err());

        tokio::fs::remove_file(&raw).await.unwrap();
        tokio::fs::write(&legacy, b"legacy").await.unwrap();

        let result = create_directory_at(&target, &[raw, legacy]).await;
        assert!(result.is_err());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn reserve_and_remove_raw_image_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("machine.raw");
        let conflicts = [
            directory.path().join("machine"),
            directory.path().join("machine.img"),
        ];

        reserve_raw_image_at(&raw, &conflicts).await.unwrap();
        assert!(raw.is_file());

        remove_image_at(&raw).await.unwrap();
        assert!(!raw.exists());
    }

    #[tokio::test]
    async fn reserve_raw_image_rejects_directory_and_legacy_image_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("machine.raw");
        let machine_dir = directory.path().join("machine");
        let legacy = directory.path().join("machine.img");
        tokio::fs::create_dir(&machine_dir).await.unwrap();

        let result = reserve_raw_image_at(&raw, &[machine_dir.clone(), legacy.clone()]).await;
        assert!(result.is_err());

        tokio::fs::remove_dir(&machine_dir).await.unwrap();
        tokio::fs::write(&legacy, b"legacy").await.unwrap();

        let result = reserve_raw_image_at(&raw, &[machine_dir, legacy]).await;
        assert!(result.is_err());
        assert!(!raw.exists());
    }

    #[tokio::test]
    async fn remove_image_rejects_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real.raw");
        let link = directory.path().join("link.raw");
        tokio::fs::write(&real, b"image").await.unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let result = remove_image_at(&link).await;

        assert!(result.is_err());
        assert!(real.exists());
        assert!(link.exists());
    }
}
