//! Disk image creation and formatting logic.

use super::DiskImageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{DiskImageFilesystem, DiskImageSource};
use crate::nspawn::sys::{log_output, CommandRunner};
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

fn copy_image_args(src_path: &std::path::Path, dest_path: &std::path::Path) -> Vec<String> {
    vec![
        "--reflink=auto".into(),
        "--sparse=always".into(),
        "--".into(),
        src_path.to_string_lossy().to_string(),
        dest_path.to_string_lossy().to_string(),
    ]
}

impl DiskImageBackend {
    pub(super) async fn create_impl(
        &self,
        name: &str,
        cmd_runner: &dyn CommandRunner,
    ) -> Result<PathBuf> {
        if self.managed_kind() != Some(crate::nspawn::adapters::storage::ManagedImageKind::Raw) {
            return Err(NspawnError::Validation(
                "Only new managed raw images can be created".into(),
            ));
        }

        if let DiskImageSource::ImportExisting { path } = &self.config.source {
            require_tool("cp")?;
            validate_import_source(path)?;
        } else if let DiskImageSource::CreateNew { fs_type, .. } = &self.config.source {
            require_tool("truncate")?;
            require_tool(fs_type.mkfs_tool())?;
            if self.config.use_partition_table {
                require_tool("sfdisk")?;
                require_tool("losetup")?;
                require_tool("udevadm")?;
            }
        }

        let dest_path = self.store.reserve_raw_image(name).await?;
        let create_result: Result<()> = async {
            match &self.config.source {
                DiskImageSource::ImportExisting { path } => {
                    let src_path = PathBuf::from(path);
                    let out = cmd_runner
                        .run("cp", copy_image_args(&src_path, &dest_path))
                        .await
                        .map_err(|e| NspawnError::Io(dest_path.clone(), e))?;
                    log_output("cp", &out);
                    if !out.status.success() {
                        return Err(NspawnError::cmd_failed(
                            "copy disk image",
                            format!("cp {} {}", src_path.display(), dest_path.display()),
                            &out,
                        ));
                    }
                }
                DiskImageSource::CreateNew { size, fs_type } => {
                    let dest_s = dest_path.to_string_lossy().to_string();
                    let out = cmd_runner
                        .run("truncate", vec!["-s".into(), size.clone(), dest_s.clone()])
                        .await?;
                    log_output("truncate", &out);
                    if !out.status.success() {
                        return Err(NspawnError::cmd_failed(
                            "truncate",
                            format!("truncate -s {} {}", size, dest_path.display()),
                            &out,
                        ));
                    }
                    self.format_image(
                        &dest_path,
                        *fs_type,
                        self.config.use_partition_table,
                        cmd_runner,
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;

        if let Err(error) = create_result {
            if let Err(cleanup_error) = self
                .store
                .remove_image(
                    name,
                    crate::nspawn::adapters::storage::ManagedImageKind::Raw,
                )
                .await
            {
                log::warn!(
                    "Failed to clean partial raw image {}: {}",
                    dest_path.display(),
                    cleanup_error
                );
            }
            return Err(error);
        }

        Ok(dest_path)
    }

    pub(super) async fn format_image(
        &self,
        path: &std::path::Path,
        fs_type: DiskImageFilesystem,
        use_partition_table: bool,
        cmd_runner: &dyn CommandRunner,
    ) -> Result<()> {
        if use_partition_table {
            format_partitioned_image(path, fs_type, cmd_runner).await
        } else {
            format_whole_image(path, fs_type, cmd_runner).await
        }
    }
}

fn validate_import_source(path: &str) -> Result<()> {
    if path.trim().is_empty() {
        return Err(NspawnError::Validation(
            "Source image path is required".into(),
        ));
    }

    let source = PathBuf::from(path);
    let metadata = std::fs::metadata(&source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            NspawnError::Validation(format!("Source image not found: {}", path))
        } else {
            NspawnError::Io(source.clone(), error)
        }
    })?;

    let file_type = metadata.file_type();
    if !file_type.is_file() && !file_type.is_block_device() {
        return Err(NspawnError::Validation(format!(
            "Source image is not a file or block device: {}",
            path
        )));
    }

    Ok(())
}

fn require_tool(name: &str) -> Result<()> {
    which::which(name)
        .map(|_| ())
        .map_err(|_| NspawnError::ToolNotFound(name.to_string()))
}

fn mkfs_args(fs_type: DiskImageFilesystem, target: impl Into<String>) -> Vec<String> {
    let force_flag = match fs_type {
        DiskImageFilesystem::Ext4 => "-F",
        DiskImageFilesystem::Xfs | DiskImageFilesystem::Btrfs => "-f",
    };
    vec![force_flag.into(), target.into()]
}

async fn format_whole_image(
    path: &std::path::Path,
    fs_type: DiskImageFilesystem,
    cmd_runner: &dyn CommandRunner,
) -> Result<()> {
    let mkfs = fs_type.mkfs_tool();
    let path_s = path.to_string_lossy().to_string();
    let out = cmd_runner.run(mkfs, mkfs_args(fs_type, path_s)).await?;
    log_output(mkfs, &out);

    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "mkfs",
            format!("{} on {}", mkfs, path.display()),
            &out,
        ));
    }

    Ok(())
}

async fn format_partitioned_image(
    path: &std::path::Path,
    fs_type: DiskImageFilesystem,
    cmd_runner: &dyn CommandRunner,
) -> Result<()> {
    let root_uuid = super::utils::get_discoverable_root_uuid();
    let path_s = path.to_string_lossy().to_string();
    let sfdisk_script = format!("label: gpt\ntype={}\n", root_uuid);

    // 1. Partition with sfdisk (stdin piped via sh -c)
    let out = cmd_runner
        .run(
            "sh",
            vec![
                "-c".into(),
                format!("printf '%s' '{}' | sfdisk {}", sfdisk_script, path_s),
            ],
        )
        .await?;
    log_output("sfdisk", &out);
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "sfdisk",
            "sfdisk gpt partition",
            &out,
        ));
    }

    // 2. Setup loop device with partition scanning
    let out = cmd_runner
        .run(
            "losetup",
            vec![
                "--find".into(),
                "--partscan".into(),
                "--show".into(),
                path_s.clone(),
            ],
        )
        .await?;
    log_output("losetup", &out);
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "losetup -P",
            "losetup --find -P --show",
            &out,
        ));
    }
    let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let result: Result<()> = async {
        let part_dev = format!("{}p1", loop_dev);

        let settle = cmd_runner
            .run("udevadm", vec!["settle".into(), "--timeout=5".into()])
            .await?;
        log_output("udevadm", &settle);
        if !settle.status.success() {
            return Err(NspawnError::cmd_failed(
                "udevadm settle",
                "udevadm settle --timeout=5",
                &settle,
            ));
        }

        if !std::path::Path::new(&part_dev).exists() {
            return Err(NspawnError::Generic(format!(
                "Timeout waiting for partition device {}. Ensure loop module supports partitions.",
                part_dev
            )));
        }

        // 3. Format partition
        let mkfs = fs_type.mkfs_tool();
        let out = cmd_runner
            .run(mkfs, mkfs_args(fs_type, part_dev.clone()))
            .await?;
        log_output(mkfs, &out);

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "mkfs",
                format!("{} on {}", mkfs, part_dev),
                &out,
            ));
        }

        Ok(())
    }
    .await;

    let _ = cmd_runner.run("losetup", vec!["-d".into(), loop_dev]).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_existing_uses_binary_safe_sparse_copy() {
        let args = copy_image_args(
            std::path::Path::new("/tmp/source image.raw"),
            std::path::Path::new("/var/lib/machines/test.raw"),
        );

        assert_eq!(
            args,
            vec![
                "--reflink=auto",
                "--sparse=always",
                "--",
                "/tmp/source image.raw",
                "/var/lib/machines/test.raw",
            ]
        );
    }

    #[test]
    fn mkfs_args_use_filesystem_specific_force_flags() {
        assert_eq!(
            mkfs_args(DiskImageFilesystem::Ext4, "/dev/loop0p1"),
            vec!["-F", "/dev/loop0p1"]
        );
        assert_eq!(
            mkfs_args(DiskImageFilesystem::Xfs, "/dev/loop0p1"),
            vec!["-f", "/dev/loop0p1"]
        );
        assert_eq!(
            mkfs_args(DiskImageFilesystem::Btrfs, "/dev/loop0p1"),
            vec!["-f", "/dev/loop0p1"]
        );
    }

    #[test]
    fn import_source_rejects_empty_path_and_directory() {
        assert!(validate_import_source("").is_err());

        let directory = tempfile::tempdir().unwrap();
        assert!(validate_import_source(directory.path().to_str().unwrap()).is_err());
    }
}
