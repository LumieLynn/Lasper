use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};

/// Enable systemd-networkd and systemd-resolved inside the container.
pub async fn enable_container_networkd(
    rootfs: &Path,
    cmd_runner: &dyn CommandRunner,
    io: &ElevatedIo,
) -> Result<()> {
    // Check systemctl exists before attempting anything
    if io
        .read_to_string(&rootfs.join("usr/bin/systemctl"))
        .await?
        .is_none()
    {
        return Ok(());
    }

    let rootfs_s = rootfs.to_string_lossy().to_string();

    let out = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.clone(),
                "--quiet".into(),
                "systemctl".into(),
                "enable".into(),
                "systemd-networkd".into(),
                "systemd-resolved".into(),
            ],
        )
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    log_output("systemctl", &out);
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "systemctl enable in container",
            format!("systemd-nspawn -D {:?} systemctl enable ...", rootfs),
            &out,
        ));
    }

    // Replace resolv.conf symlink
    let _ = cmd_runner.run(
        "systemd-nspawn",
        vec![
            "-D".into(),
            rootfs_s,
            "sh".into(),
            "-c".into(),
            "rm -f /etc/resolv.conf && ln -sf ../run/systemd/resolve/stub-resolv.conf /etc/resolv.conf".into(),
        ],
    ).await;

    Ok(())
}
