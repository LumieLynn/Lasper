//! Disk image mounting and unmounting logic (the "Armored Beast").

use super::DiskImageBackend;
use crate::nspawn::adapters::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{new_command, CommandLogged};
use std::path::{Path, PathBuf};

impl DiskImageBackend {
    pub(super) async fn mount_impl(&self, name: &str) -> Result<PathBuf> {
        let img_path = self.get_path(name);
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));
        tokio::fs::create_dir_all(&mount_point).await?;

        // 1. Primary: systemd-dissect
        let mut cmd = new_command("systemd-dissect");
        cmd.args([
            "--mount",
            &img_path.to_string_lossy(),
            &mount_point.to_string_lossy(),
        ]);
        let out = cmd.logged_output("systemd-dissect").await?;
        if out.status.success() {
            return Ok(mount_point);
        }

        let err = String::from_utf8_lossy(&out.stderr);
        log::warn!(
            "systemd-dissect failed ({}). Attempting fallback...",
            err.trim()
        );

        // 2. Fallback
        self.mount_fallback(&img_path, &mount_point).await
    }

    pub(super) async fn unmount_impl(&self, name: &str) -> Result<()> {
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));

        // 1. Try systemd-dissect
        let out = new_command("systemd-dissect")
            .arg("--umount")
            .arg(&mount_point.to_string_lossy().to_string())
            .logged_output("systemd-dissect")
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("not mounted") && !err.contains("no such file") {
                log::warn!("systemd-dissect umount failed. Forcing standard umount.");
                let _ = new_command("umount")
                    .arg(&mount_point.to_string_lossy().to_string())
                    .logged_output("umount")
                    .await;
            }
        }

        // 2. Cleanup fallbacks (nbd, loop, luks)
        self.cleanup_fallback(name).await?;

        let _ = tokio::fs::remove_dir(&mount_point).await;
        Ok(())
    }

    async fn mount_fallback(&self, img_path: &Path, mount_point: &Path) -> Result<PathBuf> {
        let out = new_command("losetup")
            .args([
                "--find",
                "--partscan",
                "--show",
                &img_path.to_string_lossy(),
            ])
            .logged_output("losetup")
            .await?;
        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "losetup",
                "losetup --find -P --show",
                &out,
            ));
        }
        let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let _ = new_command("udevadm")
            .args(["settle", "--timeout=5"])
            .logged_output("udevadm")
            .await;

        let part_p1 = format!("{}p1", loop_dev);

        let dev = if Path::new(&part_p1).exists() {
            part_p1
        } else {
            loop_dev.clone()
        };

        // Try to mount
        if !std::path::Path::new(&dev).exists() {
            let _ = new_command("losetup")
                .args(["-d", &loop_dev])
                .logged_output("losetup")
                .await;
            return Err(NspawnError::mount_failed(format!(
                "Final device {} does not exist for mounting.",
                dev
            )));
        }
        let out = new_command("mount")
            .arg(&dev)
            .arg(&mount_point.to_string_lossy().to_string())
            .logged_output("mount")
            .await?;

        if out.status.success() {
            return Ok(mount_point.to_path_buf());
        }

        // Cleanup on failure
        let _ = new_command("losetup")
            .args(["-d", &loop_dev])
            .logged_output("losetup")
            .await;

        Err(NspawnError::mount_failed("Fallback mount failed."))
    }

    async fn cleanup_fallback(&self, name: &str) -> Result<()> {
        let img_path = self.get_path(name);

        // Surgical Loop cleanup
        if let Ok(Some(loop_dev)) = super::utils::find_loop_device(&img_path).await {
            let _ = new_command("losetup")
                .arg("-d")
                .arg(&loop_dev)
                .logged_output("losetup")
                .await;
        }

        Ok(())
    }
}
