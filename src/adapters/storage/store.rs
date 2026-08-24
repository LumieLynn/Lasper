use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{DiskImageFilesystem, DiskImagePartition, MachineName};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Typed access to Lasper-managed storage paths.
#[derive(Clone, Debug, Default)]
pub struct ManagedStorageStore;

impl ManagedStorageStore {
    pub fn new() -> Self {
        Self
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

    pub async fn create_subvolume(&self, name: &str) -> Result<PathBuf> {
        let result = self
            .execute(ManagedStorageOperation::CreateSubvolume(
                CreateManagedSubvolume {
                    machine: parse_machine_name(name)?,
                },
            ))
            .await?;
        result.path.ok_or_else(|| {
            NspawnError::Runtime("managed subvolume operation returned no path".into())
        })
    }

    pub async fn remove_subvolume(&self, name: &str) -> Result<()> {
        self.execute(ManagedStorageOperation::RemoveSubvolume(
            RemoveManagedSubvolume {
                machine: parse_machine_name(name)?,
            },
        ))
        .await?;
        Ok(())
    }

    pub async fn create_raw_image(
        &self,
        name: &str,
        size: &str,
        filesystem: DiskImageFilesystem,
        partition_table: bool,
    ) -> Result<PathBuf> {
        let result = self
            .execute(ManagedStorageOperation::CreateRawImage(
                CreateManagedRawImage {
                    machine: parse_machine_name(name)?,
                    size: ManagedImageSize::parse(size)?,
                    filesystem,
                    partition_table,
                },
            ))
            .await?;
        result
            .path
            .ok_or_else(|| NspawnError::Runtime("managed image creation returned no path".into()))
    }

    pub async fn import_raw_image(&self, name: &str, source: &Path) -> Result<PathBuf> {
        let machine = parse_machine_name(name)?;
        let source_file = std::fs::File::open(source)
            .map_err(|error| NspawnError::Io(source.to_path_buf(), error))?;
        crate::adapters::storage::image_ops::validate_import_source(&source_file)?;

        crate::adapters::storage::image_ops::import_raw_image(&machine, source_file).await?;
        Ok(crate::paths::machine_raw_image(machine.as_str()))
    }

    pub async fn mount_image(
        &self,
        name: &str,
        source: ImageMountSource,
        root_partition: Option<DiskImagePartition>,
    ) -> Result<PathBuf> {
        let result = self
            .execute(ManagedStorageOperation::MountImage(MountManagedImage {
                machine: parse_machine_name(name)?,
                source,
                root_partition,
            }))
            .await?;
        result
            .path
            .ok_or_else(|| NspawnError::Runtime("managed image mount returned no path".into()))
    }

    pub async fn unmount_image(&self, name: &str, source: ImageMountSource) -> Result<()> {
        self.execute(ManagedStorageOperation::UnmountImage(UnmountManagedImage {
            machine: parse_machine_name(name)?,
            source,
        }))
        .await?;
        Ok(())
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
        execute_managed_storage_operation(operation).await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum ManagedStorageOperation {
    CreateDirectory(CreateManagedDirectory),
    RemoveDirectory(RemoveManagedDirectory),
    CreateSubvolume(CreateManagedSubvolume),
    RemoveSubvolume(RemoveManagedSubvolume),
    CreateRawImage(CreateManagedRawImage),
    MountImage(MountManagedImage),
    UnmountImage(UnmountManagedImage),
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
pub(crate) struct CreateManagedSubvolume {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveManagedSubvolume {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateManagedRawImage {
    machine: MachineName,
    size: ManagedImageSize,
    filesystem: DiskImageFilesystem,
    partition_table: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MountManagedImage {
    machine: MachineName,
    source: ImageMountSource,
    #[serde(default)]
    root_partition: Option<DiskImagePartition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnmountManagedImage {
    machine: MachineName,
    source: ImageMountSource,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "format",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ImageMountSource {
    Managed(ManagedImageKind),
    BlockDevice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct ManagedImageSize(u64);

impl ManagedImageSize {
    fn new(bytes: u64) -> Result<Self> {
        crate::adapters::storage::image_ops::validate_size_bytes(bytes)?;
        Ok(Self(bytes))
    }

    fn parse(value: &str) -> Result<Self> {
        Self::new(crate::nspawn::models::config::parse_disk_image_size(value)?)
    }
}

impl<'de> Deserialize<'de> for ManagedImageSize {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = u64::deserialize(deserializer)?;
        Self::new(bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedStorageResult {
    path: Option<PathBuf>,
}

pub(crate) async fn execute_managed_storage_operation(
    operation: ManagedStorageOperation,
) -> Result<ManagedStorageResult> {
    execute_managed_storage_operation_with_runner(operation, &DefaultCommandRunner).await
}

pub(crate) async fn execute_managed_storage_operation_with_runner(
    operation: ManagedStorageOperation,
    runner: &dyn CommandRunner,
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
        ManagedStorageOperation::CreateSubvolume(request) => {
            let path =
                crate::adapters::storage::subvolume_ops::create_subvolume(&request.machine, runner)
                    .await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::RemoveSubvolume(request) => {
            crate::adapters::storage::subvolume_ops::remove_subvolume(&request.machine, runner)
                .await?;
            Ok(ManagedStorageResult::default())
        }
        ManagedStorageOperation::CreateRawImage(request) => {
            let path = crate::adapters::storage::image_ops::create_raw_image(
                &request.machine,
                request.size.0,
                request.filesystem,
                request.partition_table,
                runner,
            )
            .await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::MountImage(request) => {
            let path = crate::adapters::storage::image_ops::mount_image(
                &request.machine,
                request.source,
                request.root_partition,
                runner,
            )
            .await?;
            Ok(ManagedStorageResult { path: Some(path) })
        }
        ManagedStorageOperation::UnmountImage(request) => {
            crate::adapters::storage::image_ops::unmount_image(
                &request.machine,
                request.source,
                runner,
            )
            .await?;
            Ok(ManagedStorageResult::default())
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

        let json = r#"{
            "operation": "create_subvolume",
            "params": {"machine": "../escape"}
        }"#;
        assert!(serde_json::from_str::<ManagedStorageOperation>(json).is_err());
    }

    #[test]
    fn operation_deserialization_rejects_invalid_image_parameters() {
        let zero_size = r#"{
            "operation": "create_raw_image",
            "params": {
                "machine": "test",
                "size": 0,
                "filesystem": "ext4",
                "partition_table": true
            }
        }"#;
        assert!(serde_json::from_str::<ManagedStorageOperation>(zero_size).is_err());

        let unknown_mount_source = r#"{
            "operation": "mount_image",
            "params": {
                "machine": "test",
                "source": {"kind": "managed", "format": "raw", "extra": true}
            }
        }"#;
        assert!(serde_json::from_str::<ManagedStorageOperation>(unknown_mount_source).is_err());

        let invalid_root_partition = r#"{
            "operation": "mount_image",
            "params": {
                "machine": "test",
                "source": {"kind": "managed", "format": "raw"},
                "root_partition": 0
            }
        }"#;
        assert!(serde_json::from_str::<ManagedStorageOperation>(invalid_root_partition).is_err());
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
