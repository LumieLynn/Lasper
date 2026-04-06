use std::path::{Path, PathBuf};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::new_command;

/// Enable systemd-networkd and systemd-resolved inside the container.
pub async fn enable_container_networkd(rootfs: &Path) -> Result<()> {
    let systemctl_path = rootfs.join("usr/bin/systemctl");
    if !systemctl_path.exists() {
        return Ok(());
    }

    let mut cmd = new_command("systemd-nspawn");
    cmd.args([
        "-q",
        "--directory",
        &rootfs.to_string_lossy(),
        "systemctl",
        "enable",
        "systemd-networkd",
        "systemd-resolved",
    ]);

    let res = cmd
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    if !res.status.success() {
        return Err(NspawnError::cmd_failed(
            "systemctl enable in container",
            format!(
                "systemd-nspawn --directory {:?} systemctl enable ...",
                rootfs
            ),
            &res,
        ));
    }

    let script = "ln -sf ../run/systemd/resolve/stub-resolv.conf /etc/resolv.conf";
    let mut script_cmd = new_command("systemd-nspawn");
    script_cmd.args([
        "-q",
        "--directory",
        &rootfs.to_string_lossy(),
        "sh",
        "-c",
        script,
    ]);
    let _ = script_cmd.output().await;

    Ok(())
}
