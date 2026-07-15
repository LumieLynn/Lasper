use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use serde::{Deserialize, Serialize};
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
            create_directory_at(&path).await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::RemoveDirectory(request) => {
            remove_directory_at(&directory_path(&request.machine)).await?;
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

async fn create_directory_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    }

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

        let result = create_directory_at(&target).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_and_remove_directory_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("machine");

        create_directory_at(&target).await.unwrap();
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
}
