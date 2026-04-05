//! Functions for generating and writing container configurations.

use super::errors::{NspawnError, Result};
use super::models::{ContainerConfig, CreateUser, NetworkMode};
use ini::Ini;
use std::path::{Path, PathBuf};
use super::utils::{new_command, new_sync_command};
use tokio::process::Command;

// ── .nspawn file generation ───────────────────────────────────────────────────

/// Generate the content of a `.nspawn` container config file using AST.
pub fn nspawn_config_content(cfg: &ContainerConfig) -> String {
    let mut conf = Ini::new();

    if cfg.nvidia_gpu {
        conf.with_section(Some("General"))
            .set("X-Lasper-Nvidia-Enabled", "true");
    }

    // ── [Exec] ────────────────────────────────────────────────────────────────
    {
        let mut exec = conf.with_section(Some("Exec"));
        exec.set("Boot", "yes");
        if cfg.wayland_socket.is_some() {
            exec.set("PrivateUsers", "no");
        }
        if cfg.full_capabilities {
            exec.set("Capability", "all");
        }
        if !cfg.hostname.is_empty() && cfg.hostname != cfg.name {
            exec.set("Hostname", &cfg.hostname);
        }
    }

    // ── [Network] ─────────────────────────────────────────────────────────────
    if let Some(mode) = &cfg.network {
        match mode {
            NetworkMode::Host => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "no");
            }
            NetworkMode::None => {
                conf.with_section(Some("Network")).set("Private", "yes");
            }
            NetworkMode::Veth => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "yes");
                let net = conf.section_mut(Some("Network")).unwrap();
                for pf in &cfg.port_forwards {
                    net.append("Port", format!("{}:{}:{}", pf.proto, pf.host, pf.container));
                }
            }
            NetworkMode::Bridge(name) => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "yes")
                    .set("Bridge", name.clone());
                let net = conf.section_mut(Some("Network")).unwrap();
                for pf in &cfg.port_forwards {
                    net.append("Port", format!("{}:{}:{}", pf.proto, pf.host, pf.container));
                }
            }
            // TODO: placeholder code for macvlan and ipvlan
            NetworkMode::MacVlan(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("MACVLAN", iface.clone());
            }
            NetworkMode::IpVlan(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("IPVLAN", iface.clone());
            }
            NetworkMode::Interface(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("Interface", iface.clone());
            }
        }
    }

    // ── [Files] ───────────────────────────────────────────────────────────────
    let has_files = !cfg.device_binds.is_empty()
        || !cfg.readonly_binds.is_empty()
        || !cfg.bind_mounts.is_empty()
        || cfg.wayland_socket.is_some();

    if has_files {
        // Use placeholder to ensure section is instantiated in Ini object before section_mut()
        conf.with_section(Some("Files")).set("__ensure_files", "");
        let files = conf.section_mut(Some("Files")).unwrap();
        files.remove("__ensure_files");

        for dev in &cfg.device_binds {
            files.append("Bind", dev.clone());
        }
        for ro in &cfg.readonly_binds {
            files.append("BindReadOnly", ro.clone());
        }
        for bm in &cfg.bind_mounts {
            if bm.readonly {
                files.append("BindReadOnly", format!("{}:{}", bm.source, bm.target));
            } else {
                files.append("Bind", format!("{}:{}", bm.source, bm.target));
            }
        }

        // Wayland and X11/GPU socket/device binding
        if let Some(socket_name) = &cfg.wayland_socket {
            // Robustly find the host's Wayland socket (handling missing ENV in sudo/su)
            let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").ok().or_else(|| {
                std::env::var("SUDO_UID").ok().map(|uid| format!("/run/user/{}", uid))
            }).unwrap_or_else(|| "/run/user/1000".to_string());

            let host_wayland_sock = format!("{}/{}", xdg_runtime, socket_name);
            files.append("Bind", format!("{}:/mnt/wayland-socket", host_wayland_sock));

            // Passthrough X11 socket and GPU rendering requirements (Mesa/Intel/AMD)
            files.append("Bind", "/tmp/.X11-unix");
            files.append("Bind", "/dev/dri");
        }
    }

    // Serialize to string
    let mut buffer = Vec::new();
    conf.write_to(&mut buffer).unwrap_or_default();
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Generate the content for a systemd service override.
pub fn systemd_override_content(
    device_binds: &[String],
    nvidia_gpu: bool,
    wayland_socket: bool,
) -> String {
    let mut content = String::from("[Service]\n");
    // Enable Cgroup delegation if passthrough is used
    if nvidia_gpu || wayland_socket {
        content.push_str("Delegate=yes\n");
    }
    for dev in device_binds {
        content.push_str(&format!("DeviceAllow={} rw\n", dev));
    }
    // Allow access to Generic GPU (Mesa/Intel/AMD)
    if wayland_socket {
        content.push_str("DeviceAllow=/dev/dri rw\n");
    }
    content
}

/// Write a systemd service override to allow devices via cgroups.
pub fn write_systemd_override(
    name: &str,
    device_binds: &[String],
    nvidia_gpu: bool,
    wayland_socket: bool,
) -> Result<()> {
    if device_binds.is_empty() && !nvidia_gpu && !wayland_socket {
        return Ok(());
    }

    let dir = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        name
    ));
    std::fs::create_dir_all(&dir).map_err(|e| NspawnError::Io(dir.clone(), e))?;

    let path = dir.join("override.conf");
    let content = systemd_override_content(device_binds, nvidia_gpu, wayland_socket);

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

    let _ = new_sync_command("systemctl")
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
        std::fs::write(&script_path, fish_script).map_err(|e| NspawnError::Io(script_path, e))?;
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

    let out = new_command("systemd-nspawn")
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
        return Err(NspawnError::cmd_failed(
            "useradd in container",
            format!("systemd-nspawn --directory {:?} useradd ...", rootfs),
            &out,
        ));
    }

    if user.sudoer {
        for group in ["sudo", "wheel"] {
            let r = new_command("systemd-nspawn")
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
                std::fs::create_dir_all(&sudoers_dir)
                    .map_err(|e| NspawnError::Io(sudoers_dir.clone(), e))?;
                let sudoers_file = sudoers_dir.join(group);
                let content = format!("%{} ALL=(ALL:ALL) ALL\n", group);
                std::fs::write(&sudoers_file, content)
                    .map_err(|e| NspawnError::Io(sudoers_file.clone(), e))?;

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
            return Err(NspawnError::cmd_failed(
                "chpasswd in container",
                format!("systemd-nspawn --directory {:?} sh -c ...", rootfs),
                &res,
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
        return Err(NspawnError::cmd_failed(
            "chpasswd for root in container",
            format!("systemd-nspawn --directory {:?} sh -c ...", rootfs),
            &res,
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
        assert!(content.contains("VirtualEthernet=no") || content.contains("VirtualEthernet = no"));
    }

    #[test]
    fn test_network_mode_none() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::None);
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("Private=yes"));
    }

    #[test]
    fn test_network_mode_veth() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::Veth);
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("VirtualEthernet=yes"));
    }

    #[test]
    fn test_network_mode_bridge() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::Bridge("virbr0".into()));
        let content = nspawn_config_content(&cfg);
        assert!(
            content.contains("VirtualEthernet = yes") || content.contains("VirtualEthernet=yes")
        );
        assert!(content.contains("Bridge = virbr0") || content.contains("Bridge=virbr0"));
    }

    #[test]
    fn test_network_mode_macvlan() {
        let mut cfg = ContainerConfig::default();
        cfg.network = Some(NetworkMode::MacVlan("eth0".into()));
        let content = nspawn_config_content(&cfg);
        assert!(content.contains("MACVLAN=eth0") || content.contains("MACVLAN = eth0"));
        assert!(content.contains("Private=yes") || content.contains("Private = yes"));
        assert!(content.contains("VirtualEthernet=no") || content.contains("VirtualEthernet = no"));
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
        assert!(content.contains("Hostname=test-host") || content.contains("Hostname = test-host"));
        assert!(
            content.contains("VirtualEthernet=yes") || content.contains("VirtualEthernet = yes")
        );
        assert!(content.contains("Port=tcp:8080:80") || content.contains("Port = tcp:8080:80"));
        assert!(content.contains("Port=tcp:2222:22") || content.contains("Port = tcp:2222:22"));
        assert!(
            content.contains("BindReadOnly=/tmp/a:/mnt/a")
                || content.contains("BindReadOnly = /tmp/a:/mnt/a")
        );
        assert!(content.contains("Bind=/tmp/b:/mnt/b") || content.contains("Bind = /tmp/b:/mnt/b"));
        assert!(content.contains("Bind=/dev/fuse") || content.contains("Bind = /dev/fuse"));
    }

    #[test]
    fn test_gui_passthrough_config() {
        let mut cfg = ContainerConfig::default();
        cfg.wayland_socket = Some("wayland-1".into());

        let content = nspawn_config_content(&cfg);

        assert!(content.contains("PrivateUsers=no") || content.contains("PrivateUsers = no"));
        assert!(content.contains("Bind=/run/user/1000/wayland-1:/mnt/wayland-socket") || content.contains("Bind = /run/user/1000/wayland-1:/mnt/wayland-socket"));
        assert!(content.contains("Bind=/dev/dri") || content.contains("Bind = /dev/dri"));
        assert!(content.contains("/mnt/wayland-socket"));
    }
}
