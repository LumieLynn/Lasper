use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::command::new_sync_command;
use ini::Ini;
use std::path::{Path, PathBuf};

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
pub fn write_systemd_override(
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
    std::fs::create_dir_all(&dir).map_err(|e| NspawnError::Io(dir.clone(), e))?;

    let path = dir.join("override.conf");
    let content =
        systemd_override_content(device_binds, nvidia_gpu, graphics_acceleration, wayland_socket);

    std::fs::write(&path, content).map_err(|e| NspawnError::Io(path, e))?;

    let out = new_sync_command("systemctl")
        .arg("daemon-reload")
        .output()
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
pub fn clone_systemd_override(source_name: &str, dest_name: &str) -> Result<()> {
    let source_dir = format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        source_name
    );
    let source_path = format!("{}/override.conf", source_dir);

    if !Path::new(&source_path).exists() {
        return Ok(());
    }

    let dest_dir = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        dest_name
    ));
    std::fs::create_dir_all(&dest_dir).map_err(|e| NspawnError::Io(dest_dir.clone(), e))?;

    let dest_path = dest_dir.join("override.conf");
    std::fs::copy(&source_path, &dest_path)
        .map_err(|e| NspawnError::Io(PathBuf::from(&dest_path), e))?;

    let _ = new_sync_command("systemctl").arg("daemon-reload").output();

    Ok(())
}
