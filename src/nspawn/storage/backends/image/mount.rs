//! Disk image mounting and unmounting logic (the "Armored Beast").

use std::path::{Path, PathBuf};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::new_command;
use crate::nspawn::storage::StorageBackend;
use super::DiskImageBackend;

impl DiskImageBackend {
    pub(super) async fn mount_impl(&self, name: &str) -> Result<PathBuf> {
        let img_path = self.get_path(name);
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));
        tokio::fs::create_dir_all(&mount_point).await?;

        // 1. Primary: systemd-dissect
        let mut cmd = new_command("systemd-dissect");
        cmd.args(["--mount", &img_path.to_string_lossy(), &mount_point.to_string_lossy()]);
        
        if let crate::nspawn::models::DiskImageSource::CreateNew { encrypted: true, passphrase: Some(_pw), .. } = &self.config.source {
            log::info!("Note: systemd-dissect might ask for LUKS passphrase if not in keyring.");
        }

        let out = cmd.output().await?;
        if out.status.success() {
            return Ok(mount_point);
        }

        let err = String::from_utf8_lossy(&out.stderr);
        log::warn!("systemd-dissect failed ({}). Attempting fallback...", err.trim());

        // 2. Fallback
        self.mount_fallback(&img_path, &mount_point).await
    }

    pub(super) async fn unmount_impl(&self, name: &str) -> Result<()> {
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));

        // 1. Try systemd-dissect
        let out = new_command("systemd-dissect")
            .arg("--umount")
            .arg(&mount_point.to_string_lossy().to_string())
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("not mounted") && !err.contains("no such file") {
                log::warn!("systemd-dissect umount failed. Forcing standard umount.");
                let _ = new_command("umount").arg(&mount_point.to_string_lossy().to_string()).output().await;
            }
        }

        // 2. Cleanup fallbacks (nbd, loop, luks)
        self.cleanup_fallback(name).await?;

        let _ = tokio::fs::remove_dir(&mount_point).await;
        Ok(())
    }

    async fn mount_fallback(&self, img_path: &Path, mount_point: &Path) -> Result<PathBuf> {
        let ext = img_path.extension().and_then(|e| e.to_str()).unwrap_or("raw");
        
        let (dev, is_temp_nbd, base_dev) = if ext == "raw" || ext == "img" {
            let out = new_command("losetup")
                .args(["--find", "--partition", "--show", &img_path.to_string_lossy()])
                .output()
                .await?;
            if !out.status.success() {
                return Err(NspawnError::cmd_failed("losetup", "losetup --find -P --show", &out));
            }
            let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Wait for devnodes deterministically
            let part_p1 = format!("{}p1", loop_dev);
            for _ in 0..10 {
                if std::path::Path::new(&part_p1).exists() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            
            let dev = if Path::new(&part_p1).exists() { part_p1 } else { loop_dev.clone() };
            (dev, false, loop_dev)
        } else {
            // Qcow2, etc.
            let _ = new_command("modprobe").args(["nbd", "max_part=16"]).output().await;
            
            // Find a free NBD device
            let nbd_dev = super::utils::find_free_nbd_device().await?
                .ok_or_else(|| NspawnError::Generic("No free NBD devices available".into()))?;

            let out = new_command("qemu-nbd")
                .args(["-c", &nbd_dev, &img_path.to_string_lossy()])
                .output()
                .await?;
            if !out.status.success() {
                return Err(NspawnError::cmd_failed("qemu-nbd connect", "qemu-nbd -c ...", &out));
            }
            
            // Wait for partitions deterministically
            let part_p1 = format!("{}p1", nbd_dev);
            for _ in 0..10 {
                if std::path::Path::new(&part_p1).exists() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            
            let dev = if Path::new(&part_p1).exists() { part_p1 } else { nbd_dev.clone() };
            (dev, true, nbd_dev)
        };

        // Check for LUKS
        let mut final_dev = dev.clone();
        let mut luks_mapped = false;
        let mut mapping_full_name = String::new();
        
        if super::utils::is_luks(Path::new(&dev)).await {
            let mapping_name = format!("lasper-{}-crypt", img_path.file_stem().unwrap_or_default().to_string_lossy());
            mapping_full_name = mapping_name.clone();
            let out = if let crate::nspawn::models::DiskImageSource::CreateNew { passphrase: Some(pw), .. } = &self.config.source {
                let mut child = new_command("cryptsetup")
                    .args(["open", "--type", "luks", "--key-file", "-", &dev, &mapping_name])
                    .stdin(std::process::Stdio::piped())
                    .spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    stdin.write_all(pw.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                }
                child.wait_with_output().await?
            } else {
                // If systemd-dissect fails on an imported LUKS image, we don't have the password in memory to fallback.
                if is_temp_nbd { let _ = new_command("qemu-nbd").args(["-d", &base_dev]).output().await; }
                else { let _ = new_command("losetup").args(["-d", &base_dev]).output().await; }
                
                return Err(NspawnError::mount_failed(
                    "Cannot fallback-mount an imported LUKS image because the passphrase is not available in the wizard context. Ensure the image is a valid GPT DDI so systemd-dissect can mount it."
                ));
            };

            if !out.status.success() {
                if is_temp_nbd { let _ = new_command("qemu-nbd").args(["-d", &base_dev]).output().await; }
                else { let _ = new_command("losetup").args(["-d", &base_dev]).output().await; }
                return Err(NspawnError::cmd_failed("cryptsetup open", "open luks device ...", &out));
            }
            final_dev = format!("/dev/mapper/{}", mapping_name);
            luks_mapped = true;
        }

        // Try to mount
        if !std::path::Path::new(&final_dev).exists() {
            if luks_mapped { let _ = new_command("cryptsetup").args(["close", &mapping_full_name]).output().await; }
            if is_temp_nbd { let _ = new_command("qemu-nbd").args(["-d", &base_dev]).output().await; }
            else { let _ = new_command("losetup").args(["-d", &base_dev]).output().await; }
            return Err(NspawnError::mount_failed(format!("Final device {} does not exist for mounting.", final_dev)));
        }
        let out = new_command("mount")
            .arg(&final_dev)
            .arg(&mount_point.to_string_lossy().to_string())
            .output()
            .await?;
            
        if out.status.success() {
            return Ok(mount_point.to_path_buf());
        }

        // Cleanup on failure
        if luks_mapped { let _ = new_command("cryptsetup").args(["close", &mapping_full_name]).output().await; }
        if is_temp_nbd { let _ = new_command("qemu-nbd").args(["-d", &base_dev]).output().await; }
        else { let _ = new_command("losetup").args(["-d", &base_dev]).output().await; }
        
        Err(NspawnError::mount_failed("Fallback mount failed."))
    }

    async fn cleanup_fallback(&self, name: &str) -> Result<()> {
        let img_path = self.get_path(name);
        
        // 1. Find and close LUKS mapping
        let mapping_name = format!("lasper-{}-crypt", img_path.file_stem().unwrap_or_default().to_string_lossy());
        let mapper_path = format!("/dev/mapper/{}", mapping_name);
        if Path::new(&mapper_path).exists() {
            let _ = new_command("cryptsetup").arg("close").arg(&mapping_name).output().await;
        }

        // 2. Surgical NBD cleanup
        if let Ok(Some(nbd_dev)) = super::utils::find_nbd_device(&img_path).await {
            let _ = new_command("qemu-nbd").args(["-d", &nbd_dev.to_string_lossy()]).output().await;
        }
        
        // 3. Surgical Loop cleanup
        if let Ok(Some(loop_dev)) = super::utils::find_loop_device(&img_path).await {
            let _ = new_command("losetup").arg("-d").arg(&loop_dev).output().await;
        }
        
        Ok(())
    }
}
