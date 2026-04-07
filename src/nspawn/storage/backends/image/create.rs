//! Disk image creation and formatting logic.

use std::path::PathBuf;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{DiskImageSource, DiskImageFormat};
use crate::nspawn::utils::new_command;
use crate::nspawn::storage::StorageBackend;
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
                tokio::fs::copy(&src_path, &dest_path).await
                    .map_err(|e| NspawnError::Io(dest_path.clone(), e))?;
            }
            DiskImageSource::CreateNew { size, fs_type, format, encrypted, passphrase } => {
                match format {
                    DiskImageFormat::Raw => {
                        let out = new_command("truncate")
                            .args(["-s", size, &dest_path.to_string_lossy()])
                            .output()
                            .await?;
                        if !out.status.success() {
                            return Err(NspawnError::cmd_failed("truncate", format!("truncate -s {} {}", size, dest_path.display()), &out));
                        }
                    }
                    _ => {
                        let fmt_str = format.extension();
                        let out = new_command("qemu-img")
                            .args(["create", "-f", fmt_str, &dest_path.to_string_lossy(), size])
                            .output()
                            .await?;
                        if !out.status.success() {
                            return Err(NspawnError::cmd_failed("qemu-img create", format!("qemu-img create -f {} {} {}", fmt_str, dest_path.display(), size), &out));
                        }
                    }
                }

                if *encrypted {
                    if let Some(pw) = passphrase {
                        self.format_encrypted(&dest_path, fs_type, pw).await?;
                    } else {
                        return Err(NspawnError::Validation("Encryption requested but no passphrase provided".into()));
                    }
                } else {
                    self.format_plain(&dest_path, fs_type, format).await?;
                }
            }
        }
        Ok(dest_path)
    }

    pub(super) async fn format_plain(&self, path: &std::path::Path, fs_type: &str, format: &DiskImageFormat) -> Result<()> {
        let root_uuid = super::utils::get_discoverable_root_uuid();
        let sfdisk_script = format!("label: gpt\ntype={}\n", root_uuid);

        match format {
            DiskImageFormat::Raw => {
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
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("sfdisk", "sfdisk gpt partition", &out));
                }

                // 2. Setup loop device with partition scanning
                let out = new_command("losetup").args(["--find", "--partition", "--show", &path.to_string_lossy()]).output().await?;
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("losetup -P", "losetup --find -P --show", &out));
                }
                let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let part_dev = format!("{}p1", loop_dev);

                // Wait for devnodes deterministically
                for _ in 0..10 {
                    if std::path::Path::new(&part_dev).exists() { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                if !std::path::Path::new(&part_dev).exists() {
                    let _ = new_command("losetup").args(["-d", &loop_dev]).output().await;
                    return Err(NspawnError::Generic(format!("Timeout waiting for partition device {}. Ensure loop module supports partitions.", part_dev)));
                }

                // 3. Format partition
                let mkfs = format!("mkfs.{}", fs_type);
                let out = new_command(&mkfs).args(["-F", &part_dev]).output().await?;
                
                // 4. Cleanup
                let _ = new_command("losetup").args(["-d", &loop_dev]).output().await;

                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("mkfs", format!("{} on {}", mkfs, part_dev), &out));
                }
            }
            _ => {
                // For non-raw (Qcow2 etc), connect via nbd
                let _ = new_command("modprobe").args(["nbd", "max_part=16"]).output().await;
                
                // Find an available nbd device surgicallly
                let nbd_dev = super::utils::find_free_nbd_device().await?
                    .ok_or_else(|| NspawnError::Generic("No free NBD devices available".into()))?;

                let out = new_command("qemu-nbd").args(["-c", &nbd_dev, &path.to_string_lossy()]).output().await?;
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("qemu-nbd connect", "qemu-nbd -c ...", &out));
                }

                // 1. Partition the NBD device
                let mut child = new_command("sfdisk")
                    .arg(&nbd_dev)
                    .stdin(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;

                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    stdin.write_all(sfdisk_script.as_bytes()).await?;
                }
                
                let out = child.wait_with_output().await?;
                if !out.status.success() {
                    let _ = new_command("qemu-nbd").args(["-d", &nbd_dev]).output().await;
                    return Err(NspawnError::cmd_failed("sfdisk", "sfdisk gpt partition on nbd", &out));
                }

                // 2. Wait for partitions
                let part_dev = format!("{}p1", nbd_dev);
                for _ in 0..10 {
                    if std::path::Path::new(&part_dev).exists() { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                if !std::path::Path::new(&part_dev).exists() {
                    let _ = new_command("qemu-nbd").args(["-d", &nbd_dev]).output().await;
                    return Err(NspawnError::Generic(format!("Timeout waiting for partition {}. Ensure host 'nbd' module is loaded with max_part > 0.", part_dev)));
                }

                // 3. Format partition
                let mkfs = format!("mkfs.{}", fs_type);
                let out = new_command(&mkfs).args(["-F", &part_dev]).output().await?;
                
                // 4. Cleanup
                let _ = new_command("qemu-nbd").args(["-d", &nbd_dev]).output().await;
                
                if !out.status.success() {
                    return Err(NspawnError::cmd_failed("mkfs over nbd partition", format!("{} on {}", mkfs, part_dev), &out));
                }
            }
        }
        Ok(())
    }
}
