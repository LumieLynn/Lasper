//! Disk image mounting and unmounting logic.

use super::DiskImageBackend;
use crate::nspawn::adapters::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};

impl DiskImageBackend {
    pub(super) async fn mount_impl(
        &self,
        name: &str,
        cmd_runner: &dyn CommandRunner,
        io: &ElevatedIo,
    ) -> Result<PathBuf> {
        let img_path = self.get_path(name);
        let img_s = img_path.to_string_lossy().to_string();
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));
        let mnt_s = mount_point.to_string_lossy().to_string();
        io.create_dir_all(&mount_point).await?;

        // 1. Primary: systemd-dissect
        let out = cmd_runner
            .run(
                "systemd-dissect",
                vec!["--mount".into(), img_s.clone(), mnt_s.clone()],
            )
            .await?;
        log_output("systemd-dissect", &out);
        if out.status.success() {
            return Ok(mount_point);
        }

        let err = String::from_utf8_lossy(&out.stderr);
        log::warn!("systemd-dissect failed ({}). Attempting fallback...", err.trim());

        // 2. Fallback
        self.mount_fallback(&img_path, &mount_point, cmd_runner).await
    }

    pub(super) async fn unmount_impl(
        &self,
        name: &str,
        cmd_runner: &dyn CommandRunner,
        io: &ElevatedIo,
    ) -> Result<()> {
        let mount_point = PathBuf::from(format!("/mnt/lasper-{}", name));
        let mnt_s = mount_point.to_string_lossy().to_string();

        // 1. Try systemd-dissect
        let out = cmd_runner
            .run("systemd-dissect", vec!["--umount".into(), mnt_s.clone()])
            .await?;
        log_output("systemd-dissect", &out);

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("not mounted") && !err.contains("no such file") {
                log::warn!("systemd-dissect umount failed. Forcing standard umount.");
                let _ = cmd_runner
                    .run("umount", vec![mnt_s])
                    .await;
            }
        }

        // 2. Cleanup fallbacks (nbd, loop, luks)
        self.cleanup_fallback(name, cmd_runner, io).await?;

        let _ = io.remove_dir_all(&mount_point).await;
        Ok(())
    }

    async fn mount_fallback(
        &self,
        img_path: &Path,
        mount_point: &Path,
        cmd_runner: &dyn CommandRunner,
    ) -> Result<PathBuf> {
        let img_s = img_path.to_string_lossy().to_string();
        let mnt_s = mount_point.to_string_lossy().to_string();

        let out = cmd_runner
            .run(
                "losetup",
                vec![
                    "--find".into(),
                    "--partscan".into(),
                    "--show".into(),
                    img_s,
                ],
            )
            .await?;
        log_output("losetup", &out);
        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "losetup",
                "losetup --find -P --show",
                &out,
            ));
        }
        let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        cmd_runner
            .run("udevadm", vec!["settle".into(), "--timeout=5".into()])
            .await?;

        let part_p1 = format!("{}p1", loop_dev);
        let dev = if Path::new(&part_p1).exists() {
            part_p1
        } else {
            loop_dev.clone()
        };

        if !std::path::Path::new(&dev).exists() {
            let _ = cmd_runner
                .run("losetup", vec!["-d".into(), loop_dev])
                .await;
            return Err(NspawnError::mount_failed(format!(
                "Final device {} does not exist for mounting.",
                dev
            )));
        }
        let out = cmd_runner
            .run("mount", vec![dev, mnt_s])
            .await?;
        log_output("mount", &out);

        if out.status.success() {
            return Ok(mount_point.to_path_buf());
        }

        let _ = cmd_runner
            .run("losetup", vec!["-d".into(), loop_dev])
            .await;

        Err(NspawnError::mount_failed("Fallback mount failed."))
    }

    async fn cleanup_fallback(
        &self,
        name: &str,
        cmd_runner: &dyn CommandRunner,
        io: &ElevatedIo,
    ) -> Result<()> {
        let img_path = self.get_path(name);
        if let Ok(Some(loop_dev)) =
            super::utils::find_loop_device(&img_path, cmd_runner, io).await
        {
            let _ = cmd_runner
                .run("losetup", vec!["-d".into(), loop_dev.to_string_lossy().to_string()])
                .await;
        }
        Ok(())
    }
}
