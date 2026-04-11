use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::CommandLogged;
use ini::Ini;
use std::path::PathBuf;

/// Generate the content for a systemd service override.
pub fn systemd_override_content(
    device_binds: &[String],
    _nvidia_gpu: bool,
    _graphics_acceleration: bool,
    wayland_socket: bool,
) -> String {
    let mut conf = Ini::new();
    conf.with_section(Some("Service")).set("__placeholder", "");
    let s = conf.section_mut(Some("Service")).unwrap();
    s.remove("__placeholder");

    // if nvidia_gpu || wayland_socket {
    //     s.insert("Delegate", "yes");
    // }
    // Note: Delegate=yes is no longer used for GPU/Wayland passthrough to maintain
    // the Principle of Least Privilege and avoid cgroup management power-leaks.

    for dev in device_binds {
        s.append("DeviceAllow", format!("{} rw", dev));
    }
    if wayland_socket {
        s.append("DeviceAllow", "/dev/dri rw");
    }
    // Note: Individual device allows (/dev/dri, /dev/mali, etc.) are now 
    // dynamically discovered and passed via device_binds.

    let mut buffer = Vec::new();
    conf.write_to(&mut buffer).unwrap_or_default();
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Write a systemd service override to allow devices via cgroups.
pub async fn write_systemd_override(
    name: &str,
    device_binds: &[String],
    nvidia_gpu: bool,
    graphics_acceleration: bool,
    wayland_socket: bool,
) -> Result<()> {
    if device_binds.is_empty() && !nvidia_gpu && !wayland_socket && !graphics_acceleration {
        return Ok(());
    }

    let dir = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        name
    ));
    let path = dir.join("override.conf");

    crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&path, |_existing| {
        let content = systemd_override_content(
            device_binds,
            nvidia_gpu,
            graphics_acceleration,
            wayland_socket,
        );
        Ok(content)
    })
    .await?;

    let out = crate::nspawn::utils::new_command("systemctl")
        .arg("daemon-reload")
        .logged_output("systemctl")
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemctl"), e))?;

    if !out.status.success() {
        log::warn!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

/// Clones a systemd service override from one container to another.
pub async fn clone_systemd_override(source_name: &str, dest_name: &str) -> Result<()> {
    let source_path = format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d/override.conf",
        source_name
    );

    if !tokio::fs::try_exists(&source_path)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let dest_path = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d/override.conf",
        dest_name
    ));

    // Transactional write for destination
    crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&dest_path, |_existing| {
        // Read source content inside the generator is slightly inefficient but safe.
        // Better: Read source first, THEN call write_locked.
        Ok(String::new()) // Placeholder
    })
    .await?;

    // Refactored for better efficiency
    let source_content = tokio::fs::read_to_string(&source_path)
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from(&source_path), e))?;

    crate::nspawn::utils::io::AsyncLockedWriter::write_locked(&dest_path, |_| Ok(source_content))
        .await?;

    let _ = crate::nspawn::utils::new_command("systemctl")
        .arg("daemon-reload")
        .logged_output("systemctl")
        .await;

    Ok(())
}
