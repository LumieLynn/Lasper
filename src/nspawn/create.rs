//! Functions for generating and writing container configurations.

use super::errors::{NspawnError, Result};
use super::models::{ContainerConfig, CreateUser, NetworkMode};
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ── .nspawn file generation ───────────────────────────────────────────────────

/// Generate the content of a `.nspawn` container config file.
pub fn nspawn_config_content(cfg: &ContainerConfig) -> String {
    let mut out = String::new();

    if cfg.nvidia_gpu {
        out.push_str("[General]\n");
        out.push_str("X-Lasper-Nvidia-Enabled=true\n\n");
    }

    // ── [Exec] ────────────────────────────────────────────────────────────────
    out.push_str("[Exec]\n");
    out.push_str("Boot=yes\n");
    if cfg.wayland_socket {
        out.push_str("PrivateUsers=no\n");
    }
    if cfg.full_capabilities {
        out.push_str("Capability=all\n");
    }
    if !cfg.hostname.is_empty() && cfg.hostname != cfg.name {
        out.push_str(&format!("Hostname={}\n", cfg.hostname));
    }
    out.push('\n');

    // ── [Network] ─────────────────────────────────────────────────────────────
    if let Some(mode) = &cfg.network {
        out.push_str("[Network]\n");
        match mode {
            NetworkMode::Host => {
                // Offset machinectl's default veth by disabling it
                out.push_str("VirtualEthernet=no\n\n");
            }
            NetworkMode::None => {
                // Enable private network namespace and remove default veth
                out.push_str("Private=yes\n\n");
            }
            NetworkMode::Veth => {
                out.push_str("VirtualEthernet=yes\n");
                for pf in &cfg.port_forwards {
                    out.push_str(&format!("Port={}:{}:{}\n", pf.proto, pf.host, pf.container));
                }
                out.push('\n');
            }
            NetworkMode::Bridge(name) => {
                out.push_str("VirtualEthernet=yes\n");
                out.push_str(&format!("Bridge={name}\n"));
                for pf in &cfg.port_forwards {
                    out.push_str(&format!("Port={}:{}:{}\n", pf.proto, pf.host, pf.container));
                }
                out.push('\n');
            }
            // TODO: placeholder code for macvlan and ipvlan
            NetworkMode::MacVlan(iface) => {
                out.push_str("Private=yes\n");
                out.push_str("VirtualEthernet=no\n");
                out.push_str(&format!("MACVLAN={}\n\n", iface));
            }
            NetworkMode::IpVlan(iface) => {
                out.push_str("Private=yes\n");
                out.push_str("VirtualEthernet=no\n");
                out.push_str(&format!("IPVLAN={}\n\n", iface));
            }
            NetworkMode::Interface(iface) => {
                out.push_str("Private=yes\n");
                out.push_str("VirtualEthernet=no\n");
                out.push_str(&format!("Interface={}\n\n", iface));
            }
        }
    }

    // ── [Files] ───────────────────────────────────────────────────────────────
    let has_files = !cfg.device_binds.is_empty()
        || !cfg.readonly_binds.is_empty()
        || !cfg.bind_mounts.is_empty();
    if has_files {
        out.push_str("[Files]\n");
        for dev in &cfg.device_binds {
            out.push_str(&format!("Bind={dev}\n"));
        }
        for ro in &cfg.readonly_binds {
            out.push_str(&format!("BindReadOnly={ro}\n"));
        }
        for bm in &cfg.bind_mounts {
            if bm.readonly {
                out.push_str(&format!("BindReadOnly={}:{}\n", bm.source, bm.target));
            } else {
                out.push_str(&format!("Bind={}:{}\n", bm.source, bm.target));
            }
        }
        out.push('\n');
    }

    out
}

/// Generate the content for a systemd service override.
pub fn systemd_override_content(device_binds: &[String], nvidia_gpu: bool) -> String {
    let mut content = String::from("[Service]\n");
    if nvidia_gpu {
        // Enable Cgroup delegation for nested containers if GPU is used
        content.push_str("Delegate=yes\n");
    }
    for dev in device_binds {
        content.push_str(&format!("DeviceAllow={} rw\n", dev));
    }
    content
}

/// Write a systemd service override to allow devices via cgroups.
pub fn write_systemd_override(name: &str, device_binds: &[String], nvidia_gpu: bool) -> Result<()> {
    if device_binds.is_empty() && !nvidia_gpu {
        return Ok(());
    }

    let dir = PathBuf::from(format!("/etc/systemd/system/systemd-nspawn@{}.service.d", name));
    std::fs::create_dir_all(&dir).map_err(|e| NspawnError::Io(dir.clone(), e))?;

    let path = dir.join("override.conf");
    let content = systemd_override_content(device_binds, nvidia_gpu);

    std::fs::write(&path, content).map_err(|e| NspawnError::Io(path, e))?;

    let out = std::process::Command::new("systemctl")
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

    let dest_dir = PathBuf::from(format!("/etc/systemd/system/systemd-nspawn@{}.service.d", dest_name));
    std::fs::create_dir_all(&dest_dir).map_err(|e| NspawnError::Io(dest_dir.clone(), e))?;

    let dest_path = dest_dir.join("override.conf");
    std::fs::copy(&source_path, &dest_path)
        .map_err(|e| NspawnError::Io(PathBuf::from(&dest_path), e))?;

    let _ = std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .output();

    Ok(())
}

/// Clones an .nspawn configuration file from one container to another.
pub fn clone_nspawn_config(source_name: &str, dest_name: &str) -> Result<()> {
    let source_path = format!("/etc/systemd/nspawn/{}.nspawn", source_name);

    if !Path::new(&source_path).exists() {
        return Ok(());
    }

    let dest_path = format!("/etc/systemd/nspawn/{}.nspawn", dest_name);
    std::fs::copy(&source_path, &dest_path)
        .map_err(|e| NspawnError::Io(PathBuf::from(&dest_path), e))?;

    Ok(())
}

/// Sets up the target user's shell environments with exported Wayland variables.
pub async fn setup_wayland_shell_env(rootfs: &Path, user: &CreateUser) -> Result<()> {
    let home_dir = if user.username == "root" {
        "/root".to_string()
    } else {
        format!("/home/{}", user.username)
    };
    let env_script_path = format!("{}/.wayland-env", home_dir);

    let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

    let script_content = format!(
        r#"
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export WAYLAND_DISPLAY=wayland-socket
export DISPLAY={}
mkdir -p "$XDG_RUNTIME_DIR"
ln -sf /mnt/wayland-socket "$XDG_RUNTIME_DIR/wayland-socket"
"#,
        host_display
    );

    let full_path = rootfs.join(env_script_path.trim_start_matches('/'));
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
    }
    std::fs::write(&full_path, script_content).map_err(|e| NspawnError::Io(full_path, e))?;

    let shell = user.shell.as_str();
    let rc_file = if shell.ends_with("zsh") {
        ".zshrc"
    } else if shell.ends_with("fish") {
        let fish_dir = rootfs.join(format!(
            "{}/.config/fish/conf.d",
            home_dir.trim_start_matches('/')
        ));
        std::fs::create_dir_all(&fish_dir).map_err(|e| NspawnError::Io(fish_dir.clone(), e))?;
        let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let fish_script = format!(
            r#"
set -lx XDG_RUNTIME_DIR /run/user/(id -u)
set -lx WAYLAND_DISPLAY wayland-socket
set -lx DISPLAY {}
mkdir -p $XDG_RUNTIME_DIR
ln -sf /mnt/wayland-socket $XDG_RUNTIME_DIR/wayland-socket
"#,
            host_display
        );
        let script_path = fish_dir.join("wayland-env.fish");
        std::fs::write(&script_path, fish_script)
            .map_err(|e| NspawnError::Io(script_path, e))?;
        return Ok(());
    } else {
        ".bashrc"
    };

    let rc_full_path = rootfs.join(format!("{}/{}", home_dir.trim_start_matches('/'), rc_file));
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&rc_full_path)
    {
        writeln!(f, "\n[ -f ~/.wayland-env ] && source ~/.wayland-env")
            .map_err(|e| NspawnError::Io(rc_full_path, e))?;
    }

    Ok(())
}

/// Create a user inside the container rootfs via `systemd-nspawn --directory … useradd`.
pub async fn create_user_in_container(rootfs: &Path, user: &CreateUser) -> Result<()> {
    let shell = if user.shell.is_empty() {
        "/bin/bash"
    } else {
        user.shell.as_str()
    };

    let out = Command::new("systemd-nspawn")
        .args([
            "--directory",
            &rootfs.to_string_lossy(),
            "useradd",
            "-m",
            "-s",
            shell,
            &user.username,
        ])
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    if !out.status.success() {
        return Err(NspawnError::CommandFailed(
            "useradd in container".into(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    if user.sudoer {
        for group in ["sudo", "wheel"] {
            let r = Command::new("systemd-nspawn")
                .args([
                    "--directory",
                    &rootfs.to_string_lossy(),
                    "usermod",
                    "-aG",
                    group,
                    &user.username,
                ])
                .output()
                .await;
            if r.map(|o| o.status.success()).unwrap_or(false) {
                let sudoers_dir = rootfs.join("etc/sudoers.d");
                std::fs::create_dir_all(&sudoers_dir).map_err(|e| NspawnError::Io(sudoers_dir.clone(), e))?;
                let sudoers_file = sudoers_dir.join(group);
                let content = format!("%{} ALL=(ALL:ALL) ALL\n", group);
                std::fs::write(&sudoers_file, content).map_err(|e| NspawnError::Io(sudoers_file.clone(), e))?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mut perms) = std::fs::metadata(&sudoers_file).map(|m| m.permissions())
                    {
                        perms.set_mode(0o440);
                        let _ = std::fs::set_permissions(&sudoers_file, perms);
                    }
                }
                break;
            }
        }
    }

    if !user.password.is_empty() {
        let script = format!("echo '{}:{}' | chpasswd", user.username, user.password);
        let mut cmd = Command::new("systemd-nspawn");
        cmd.args([
            "-q",
            "--directory",
            &rootfs.to_string_lossy(),
            "sh",
            "-c",
            &script,
        ]);

        let res = cmd
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
        if !res.status.success() {
            return Err(NspawnError::CommandFailed(
                "chpasswd in container".into(),
                String::from_utf8_lossy(&res.stderr).trim().to_string(),
            ));
        }
    }

    Ok(())
}

/// Set the root password via `chpasswd` inside the container.
pub async fn set_root_password(rootfs: &Path, password: &str) -> Result<()> {
    if password.is_empty() {
        return Ok(());
    }
    let script = format!("echo 'root:{}' | chpasswd", password);
    let mut cmd = Command::new("systemd-nspawn");
    cmd.args([
        "-q",
        "--directory",
        &rootfs.to_string_lossy(),
        "sh",
        "-c",
        &script,
    ]);

    let res = cmd
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    if !res.status.success() {
        return Err(NspawnError::CommandFailed(
            "chpasswd for root in container".into(),
            String::from_utf8_lossy(&res.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// Enable systemd-networkd and systemd-resolved inside the container.
pub async fn enable_container_networkd(rootfs: &Path) -> Result<()> {
    let systemctl_path = rootfs.join("usr/bin/systemctl");
    if !systemctl_path.exists() {
        return Ok(());
    }

    let mut cmd = Command::new("systemd-nspawn");
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
        return Err(NspawnError::CommandFailed(
            "systemctl enable in container".into(),
            String::from_utf8_lossy(&res.stderr).trim().to_string(),
        ));
    }

    let script = "ln -sf ../run/systemd/resolve/stub-resolv.conf /etc/resolv.conf";
    let mut script_cmd = Command::new("systemd-nspawn");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{BindMount, PortForward};

    #[test]
    fn test_network_mode_host() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::Host);
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("[Network]\nVirtualEthernet=no\n"));
    }

    #[test]
    fn test_network_mode_none() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::None);
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("[Network]\nPrivate=yes\n\n"));
    }

    #[test]
    fn test_network_mode_veth() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::Veth);
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("[Network]\nVirtualEthernet=yes\n"));
    }

    #[test]
    fn test_network_mode_bridge() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::Bridge("virbr0".into()));
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("[Network]\nVirtualEthernet=yes\nBridge=virbr0\n"));
    }

    #[test]
    fn test_network_mode_macvlan() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::MacVlan("eth0".into()));
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("MACVLAN=eth0"));
        assert!(content.contains("Private=yes"));
        assert!(content.contains("VirtualEthernet=no"));
    }

    #[test]
    fn test_nvidia_passthrough_no_device_allow() {
        let mut cfg = ContainerConfig::default();
        cfg.device_binds = vec!["/dev/nvidia0".into(), "/dev/nvidiactl".into()];
        cfg.full_capabilities = true;
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("Capability=all"));
        assert!(!content.contains("DeviceAllow="));
    }

    #[test]
    fn test_complex_config() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test-comp".into();
        cfg.hostname = "test-host".into();
        cfg.network = Some(NetworkMode::Veth);
        cfg.port_forwards = vec![
            PortForward {
                host: 8080,
                container: 80,
                proto: "tcp".into(),
            },
            PortForward {
                host: 2222,
                container: 22,
                proto: "tcp".into(),
            },
        ];
        cfg.bind_mounts = vec![
            BindMount {
                source: "/tmp/a".into(),
                target: "/mnt/a".into(),
                readonly: true,
            },
            BindMount {
                source: "/tmp/b".into(),
                target: "/mnt/b".into(),
                readonly: false,
            },
        ];
        cfg.device_binds = vec!["/dev/fuse".into()];

        let content = nspawn_config_content(&cfg);
        assert!(content.contains("Hostname=test-host"));
        assert!(content.contains("VirtualEthernet=yes"));
        assert!(content.contains("Port=tcp:8080:80"));
        assert!(content.contains("Port=tcp:2222:22"));
        assert!(content.contains("BindReadOnly=/tmp/a:/mnt/a"));
        assert!(content.contains("Bind=/tmp/b:/mnt/b"));
        assert!(content.contains("Bind=/dev/fuse"));
    }

    #[test]
    fn test_gui_passthrough_config() {
        let mut cfg = ContainerConfig::default();
        cfg.wayland_socket = true;

        let content = nspawn_config_content(&cfg);

        assert!(content.contains("PrivateUsers=no"));
        assert!(!content.contains("Environment=WAYLAND_DISPLAY="));
        assert!(!content.contains("Environment=DISPLAY="));
    }
}
