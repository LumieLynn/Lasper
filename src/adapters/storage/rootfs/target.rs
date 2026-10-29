use crate::adapters::storage::rootfs::RootfsError;
use crate::domain::machine::MachineName;
use std::path::{Path, PathBuf};

/// A rootfs location derived only from validated managed resource identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RootfsTarget {
    Machine {
        machine: MachineName,
    },
    ImageMount {
        machine: MachineName,
    },
    RawMount {
        machine: MachineName,
        mount_id: uuid::Uuid,
    },
}

impl RootfsTarget {
    pub(crate) fn from_provisioned_path(name: &str, path: &Path) -> Result<Self, RootfsError> {
        let machine = MachineName::new(name)
            .map_err(|error| RootfsError::InvalidTarget(error.to_string()))?;
        if path == crate::paths::machine_root(machine.as_str()) {
            return Ok(Self::Machine { machine });
        }
        if path == crate::paths::machine_image_mount(machine.as_str()) {
            return Ok(Self::ImageMount { machine });
        }
        Err(RootfsError::InvalidTarget(format!(
            "unmanaged rootfs path: {}",
            path.display()
        )))
    }

    pub(crate) fn path(&self) -> PathBuf {
        match self {
            Self::Machine { machine } => crate::paths::machine_root(machine.as_str()),
            Self::ImageMount { machine } => crate::paths::machine_image_mount(machine.as_str()),
            Self::RawMount { machine, mount_id } => crate::paths::rootfs_mounts_dir()
                .join(format!("lasper-dissect-{machine}-{mount_id}")),
        }
    }

    pub(crate) fn supports_raw_fallback(&self) -> bool {
        matches!(self, Self::Machine { .. })
    }

    pub(crate) async fn existing_directory(&self) -> Result<Option<PathBuf>, RootfsError> {
        let path = self.path();
        validate_directory(&path)
            .await
            .map(|present| present.then_some(path))
    }

    pub(crate) async fn required_directory(&self) -> Result<PathBuf, RootfsError> {
        self.existing_directory().await?.ok_or_else(|| {
            RootfsError::InvalidTarget(format!(
                "managed rootfs directory does not exist: {}",
                self.path().display()
            ))
        })
    }
}

pub(super) async fn validate_directory(path: &Path) -> Result<bool, RootfsError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(RootfsError::Io(path.to_path_buf(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RootfsError::InvalidTarget(format!(
            "non-directory or symlink rootfs target: {}",
            path.display()
        )));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioned_target_accepts_only_exact_managed_paths() {
        assert!(matches!(
            RootfsTarget::from_provisioned_path("test", &crate::paths::machine_root("test"))
                .unwrap(),
            RootfsTarget::Machine { .. }
        ));
        assert!(matches!(
            RootfsTarget::from_provisioned_path("test", &crate::paths::machine_image_mount("test"))
                .unwrap(),
            RootfsTarget::ImageMount { .. }
        ));
        assert!(RootfsTarget::from_provisioned_path("test", Path::new("/tmp/test")).is_err());
    }

    #[tokio::test]
    async fn rootfs_directory_validation_rejects_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let link = directory.path().join("link");
        tokio::fs::create_dir(&real).await.unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(validate_directory(&link).await.is_err());
    }
}
