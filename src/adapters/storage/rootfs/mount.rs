use crate::adapters::process::{log_output, CommandRunner, ROOTFS_COMMAND_TIMEOUT};
use crate::adapters::storage::rootfs::target::validate_directory;
use crate::adapters::storage::rootfs::{RootfsError, RootfsTarget};
use crate::domain::machine::MachineName;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The transaction retains this mount token until explicit unmount completes.
/// It cannot be constructed from an arbitrary directory or a caller-supplied mount id.
pub(crate) struct RawRootfsMount {
    machine: MachineName,
    mount_id: uuid::Uuid,
}

impl RawRootfsMount {
    pub(crate) async fn mount(
        machine: &MachineName,
        runner: &dyn CommandRunner,
    ) -> Result<Option<Self>, RootfsError> {
        let mount_id = uuid::Uuid::new_v4();
        if mount_managed_raw_at(machine, &mount_id, runner).await? {
            Ok(Some(Self {
                machine: machine.clone(),
                mount_id,
            }))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn target(&self) -> RootfsTarget {
        RootfsTarget::RawMount {
            machine: self.machine.clone(),
            mount_id: self.mount_id,
        }
    }

    pub(crate) async fn unmount(&self, runner: &dyn CommandRunner) -> Result<(), RootfsError> {
        unmount_managed_raw_at(&self.machine, &self.mount_id, runner).await
    }
}

async fn mount_managed_raw_at(
    machine: &MachineName,
    mount_id: &uuid::Uuid,
    runner: &dyn CommandRunner,
) -> Result<bool, RootfsError> {
    let image = crate::paths::machine_raw_image(machine.as_str());
    let metadata = match tokio::fs::symlink_metadata(&image).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(RootfsError::Io(image, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RootfsError::InvalidTarget(format!(
            "Refusing non-file or symlink managed raw image: {}",
            image.display()
        )));
    }

    let parent = crate::paths::rootfs_mounts_dir();
    let application_cache = parent.parent().ok_or_else(|| {
        RootfsError::InvalidTarget("managed rootfs mount parent has no parent directory".into())
    })?;
    ensure_private_directory(application_cache).await?;
    ensure_private_directory(&parent).await?;

    let mount_point = RootfsTarget::RawMount {
        machine: machine.clone(),
        mount_id: *mount_id,
    }
    .path();
    tokio::fs::create_dir(&mount_point)
        .await
        .map_err(|error| RootfsError::Io(mount_point.clone(), error))?;
    tokio::fs::set_permissions(&mount_point, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| RootfsError::Io(mount_point.clone(), error))?;

    let output = match runner
        .run_bounded(
            "systemd-dissect",
            vec![
                "--mount".into(),
                image.to_string_lossy().to_string(),
                mount_point.to_string_lossy().to_string(),
            ],
            ROOTFS_COMMAND_TIMEOUT,
        )
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio::fs::remove_dir(&mount_point).await;
            return Err(RootfsError::Io(PathBuf::from("systemd-dissect"), error));
        }
    };
    log_output("systemd-dissect", &output);
    if output.status.success() {
        return Ok(true);
    }

    let _ = tokio::fs::remove_dir(&mount_point).await;
    Err(RootfsError::command_failed(
        "mount managed raw image for configuration",
        format!(
            "systemd-dissect --mount {} {}",
            image.display(),
            mount_point.display()
        ),
        &output,
    ))
}

async fn ensure_private_directory(path: &Path) -> Result<(), RootfsError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(path)
                .await
                .map_err(|error| RootfsError::Io(path.to_path_buf(), error))?;
            tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| RootfsError::Io(path.to_path_buf(), error))?
        }
        Err(error) => return Err(RootfsError::Io(path.to_path_buf(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RootfsError::InvalidTarget(format!(
            "Refusing unsafe managed rootfs mount directory: {}",
            path.display()
        )));
    }
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| RootfsError::Io(path.to_path_buf(), error))
}

async fn unmount_managed_raw_at(
    machine: &MachineName,
    mount_id: &uuid::Uuid,
    runner: &dyn CommandRunner,
) -> Result<(), RootfsError> {
    let mount_point = RootfsTarget::RawMount {
        machine: machine.clone(),
        mount_id: *mount_id,
    }
    .path();
    if !validate_directory(&mount_point).await? {
        return Err(RootfsError::InvalidTarget(format!(
            "raw mount directory is missing: {}",
            mount_point.display()
        )));
    }
    let mount_point_string = mount_point.to_string_lossy().to_string();
    let output = runner
        .run_bounded(
            "systemd-dissect",
            vec!["--umount".into(), mount_point_string.clone()],
            ROOTFS_COMMAND_TIMEOUT,
        )
        .await
        .map_err(|error| RootfsError::Io(PathBuf::from("systemd-dissect"), error))?;
    log_output("systemd-dissect --umount", &output);

    if !output.status.success() {
        let fallback = runner
            .run_bounded("umount", vec![mount_point_string], ROOTFS_COMMAND_TIMEOUT)
            .await
            .map_err(|error| RootfsError::Io(PathBuf::from("umount"), error))?;
        log_output("umount", &fallback);
        if !fallback.status.success() {
            return Err(RootfsError::command_failed(
                "unmount managed raw image configuration mount",
                format!("umount {}", mount_point.display()),
                &fallback,
            ));
        }
    }

    tokio::fs::remove_dir(&mount_point)
        .await
        .map_err(|error| RootfsError::Io(mount_point, error))
}
