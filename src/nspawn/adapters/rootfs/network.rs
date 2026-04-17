use std::path::{Path, PathBuf};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{new_command, CommandLogged};

/// Enable systemd-networkd and systemd-resolved inside the container.
pub async fn enable_container_networkd(rootfs: &Path) -> Result<()> {
    let systemctl_path = rootfs.join("usr/bin/systemctl");
    if !tokio::fs::try_exists(&systemctl_path).await.unwrap_or(false) {
        return Ok(());
    }

    let mut cmd = new_command("systemctl");
    cmd.args([
        "--root",
        &rootfs.to_string_lossy(),
        "enable",
        "systemd-networkd",
        "systemd-resolved",
    ]);

    let res = cmd
        .logged_output("systemctl")
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemctl"), e))?;
    if !res.status.success() {
        return Err(NspawnError::cmd_failed(
            "systemctl enable in container",
            format!(
                "systemctl --root {:?} enable ...",
                rootfs
            ),
            &res,
        ));
    }

    let resolv_conf = rootfs.join("etc/resolv.conf");
    let _ = tokio::fs::remove_file(&resolv_conf).await;
    let _ = tokio::fs::symlink("../run/systemd/resolve/stub-resolv.conf", &resolv_conf).await;

    Ok(())
}
