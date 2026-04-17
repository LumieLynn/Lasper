//! Disk image creation and formatting logic.

use std::path::PathBuf;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::DiskImageSource;
use crate::nspawn::sys::{new_command, log_output, CommandLogged};
use crate::nspawn::adapters::storage::StorageBackend;
use super::DiskImageBackend;

impl DiskImageBackend {
    pub(super) async fn create_impl(&self, name: &str) -> Result<PathBuf> {
        let dest_path = self.get_path(name);

        match &self.config.source {
            DiskImageSource::ImportExisting { path } => {
                let src_path = PathBuf::from(path);
                if !src_path.exists() {
                    return Err(NspawnError::Validation(format!("Source image not found: {}", path)));
                }
                log::info!("Importing existing image from {} to {}", src_path.display(), dest_path.display());
                crate::nspawn::sys::io::AsyncLockedWriter::atomic_copy(&src_path, &dest_path).await?;
            }
            DiskImageSource::CreateNew { size, fs_type } => {
                let out = new_command("truncate")
                    .args(["-s", size, &dest_path.to_string_lossy()])
                    .logged_output("truncate")
                    .await?;
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("truncate", format!("truncate -s {} {}", size, dest_path.display()), &out));
                }

                self.format_plain(&dest_path, fs_type).await?;
            }
        }
        Ok(dest_path)
    }

    pub(super) async fn format_plain(&self, path: &std::path::Path, fs_type: &str) -> Result<()> {
        let root_uuid = super::utils::get_discoverable_root_uuid();
        let sfdisk_script = format!("label: gpt\ntype={}\n", root_uuid);

        // 1. Partition with sfdisk
                let mut child = new_command("sfdisk")
                    .arg(path)
                    .stdin(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;

                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    stdin.write_all(sfdisk_script.as_bytes()).await?;
                }

                let out = child.wait_with_output().await?;
                log_output("sfdisk", &out);
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("sfdisk", "sfdisk gpt partition", &out));
                }

                // 2. Setup loop device with partition scanning
                let out = new_command("losetup").args(["--find", "--partscan", "--show", &path.to_string_lossy()]).logged_output("losetup").await?;
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("losetup -P", "losetup --find -P --show", &out));
                }
                let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let part_dev = format!("{}p1", loop_dev);

                let _ = new_command("udevadm").args(["settle", "--timeout=5"]).logged_output("udevadm").await;

                if !std::path::Path::new(&part_dev).exists() {
                    let _ = new_command("losetup").args(["-d", &loop_dev]).logged_output("losetup").await;
                    return Err(NspawnError::Generic(format!("Timeout waiting for partition device {}. Ensure loop module supports partitions.", part_dev)));
                }

                // 3. Format partition
                let mkfs = format!("mkfs.{}", fs_type);
                let force_flag = match fs_type {
                    "xfs" => "-f",
                    _ => "-F", // ext2/ext3/ext4
                };
                let out = new_command(&mkfs).args([force_flag, &part_dev]).logged_output(&mkfs).await?;
                
                // 4. Cleanup
                let _ = new_command("losetup").args(["-d", &loop_dev]).logged_output("losetup").await;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed("mkfs", format!("{} on {}", mkfs, part_dev), &out));
        }

        Ok(())
    }
}
