//! Disk image creation and formatting logic.

use super::DiskImageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::DiskImageSource;
use crate::nspawn::sys::{log_output, CommandRunner};
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
            if !PathBuf::from(path).exists() {
                return Err(NspawnError::Validation(format!(
                    "Source image not found: {}",
                    path
                )));
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
                    self.format_plain(&dest_path, fs_type, cmd_runner).await?;
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

    pub(super) async fn format_plain(
        &self,
        path: &std::path::Path,
        fs_type: &str,
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
        let part_dev = format!("{}p1", loop_dev);

        // Wait for udev
        cmd_runner
            .run("udevadm", vec!["settle".into(), "--timeout=5".into()])
            .await?;

        if !std::path::Path::new(&part_dev).exists() {
            let _ = cmd_runner
                .run("losetup", vec!["-d".into(), loop_dev.clone()])
                .await;
            return Err(NspawnError::Generic(format!(
                "Timeout waiting for partition device {}. Ensure loop module supports partitions.",
                part_dev
            )));
        }

        // 3. Format partition
        let mkfs = format!("mkfs.{}", fs_type);
        let force_flag = match fs_type {
            "xfs" => "-f",
            _ => "-F",
        };
        let out = cmd_runner
            .run(&mkfs, vec![force_flag.into(), part_dev.clone()])
            .await?;
        log_output(&mkfs, &out);

        // 4. Cleanup
        let _ = cmd_runner.run("losetup", vec!["-d".into(), loop_dev]).await;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "mkfs",
                format!("{} on {}", mkfs, part_dev),
                &out,
            ));
        }

        Ok(())
    }
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
}
