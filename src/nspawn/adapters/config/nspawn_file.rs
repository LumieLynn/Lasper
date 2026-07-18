use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, NspawnConfigSpec, PrivateUsersMode};
use ini::Ini;
use std::path::{Path, PathBuf};

pub(crate) fn escape_nspawn_bind_path(path: &str) -> String {
    path.replace('\\', "\\\\").replace(':', "\\:")
}

pub(crate) fn parse_nspawn_bind_paths(value: &str) -> Option<(String, String)> {
    let mut fields = vec![String::new()];
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.peek().copied() {
                Some('\\' | ':') => {
                    fields.last_mut()?.push(chars.next()?);
                }
                Some(_) | None => fields.last_mut()?.push('\\'),
            },
            ':' => fields.push(String::new()),
            _ => fields.last_mut()?.push(character),
        }
    }

    let source = fields.first()?.clone();
    let destination = fields.get(1).cloned().unwrap_or_else(|| source.clone());
    Some((source, destination))
}

/// Raw content of a `.nspawn` config file from `/etc/systemd/nspawn/`.
pub struct NspawnConfig {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub content: String,
}

/// Validates a container name matches systemd machine name constraints.
/// Defense-in-depth: the wizard UI already validates this, but backend
/// must not trust inputs blindly in case of restricted-sudo environments.
#[cfg(test)]
fn validate_machine_name(name: &str) -> Result<()> {
    crate::nspawn::models::MachineName::new(name)
        .map(|_| ())
        .map_err(|error| NspawnError::Validation(error.to_string()))
}

impl NspawnConfig {
    pub fn default_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name))
    }

    /// Check if the NVIDIA GPU passthrough is enabled for this container.
    pub fn is_gpu_enabled(&self) -> bool {
        let conf = match Ini::load_from_str(&self.content) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // Read legacy marker locations as well as the current [Files] location.
        let enabled_msg = "X-Lasper-Nvidia-Enabled";
        let in_files = conf.get_from(Some("Files"), enabled_msg);
        let in_general = conf.get_from(Some("General"), enabled_msg);
        let in_global = conf.get_from(None::<&str>, enabled_msg);

        in_files
            .or(in_general)
            .or(in_global)
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false)
    }

    /// Scans the raw content for markers and removes the block.
    pub fn purge_nvidia_block(content: &str) -> Result<(String, Vec<String>)> {
        let lines: Vec<&str> = content.lines().collect();
        let mut start_idx = None;
        let mut end_idx = None;
        let mut death_list = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("X-Lasper-Nvidia-Begin=") {
                if start_idx.is_some() {
                    return Err(NspawnError::Runtime(
                        "Duplicate X-Lasper-Nvidia-Begin marker".into(),
                    ));
                }
                start_idx = Some(i);
            } else if trimmed.starts_with("X-Lasper-Nvidia-End=") {
                if end_idx.is_some() {
                    return Err(NspawnError::Runtime(
                        "Duplicate X-Lasper-Nvidia-End marker".into(),
                    ));
                }
                end_idx = Some(i);
            }
        }

        match (start_idx, end_idx) {
            (Some(s), Some(e)) => {
                if s > e {
                    return Err(NspawnError::Runtime("Markers out of order".into()));
                }
                // Extract paths from common entries in this block
                for line in &lines[s + 1..e] {
                    let line = line.trim();
                    if line.starts_with("Bind=") || line.starts_with("BindReadOnly=") {
                        if let Some(val) = line.split_once('=').map(|x| x.1) {
                            death_list.push(val.to_string());
                        }
                    }
                }
                // Reconstruct content excluding the block
                let mut new_lines = Vec::new();
                for (i, line) in lines.iter().enumerate() {
                    if i < s || i > e {
                        new_lines.push(*line);
                    }
                }
                Ok((new_lines.join("\n"), death_list))
            }
            (None, None) => Ok((content.to_string(), Vec::new())),
            _ => Err(NspawnError::Runtime(
                "Incomplete markers found: one is missing".into(),
            )),
        }
    }

    /// Pure AST surgery on a string content.
    pub fn apply_gpu_passthrough_to_content(
        content: String,
        new_state: &crate::nspawn::platform::nvidia::NvidiaState,
        _death_list: &[String],
    ) -> Result<String> {
        // 1. Purge existing block using markers (preserves everything else)
        let (clean_content, _extracted_deaths) = Self::purge_nvidia_block(&content)?;

        // 2. Read-only INI parse for legacy dedup detection
        let mut lines_to_remove: Vec<String> = Vec::new();
        if let Ok(conf) = Ini::load_from_str(&clean_content) {
            if let Some(files_section) = conf.section(Some("Files")) {
                for (key, value) in files_section.iter() {
                    let Some((host_path, container_path)) = parse_nspawn_bind_paths(value) else {
                        continue;
                    };

                    let is_in_binds = new_state
                        .binds
                        .iter()
                        .any(|b| b.host_path == host_path || b.container_path == container_path);

                    if (key == "Bind" || key == "BindReadOnly") && is_in_binds {
                        if key == "Bind" {
                            lines_to_remove.push(format!("Bind={}", value));
                        } else {
                            lines_to_remove.push(format!("BindReadOnly={}", value));
                        }
                    }
                }
            }
        }

        // 3. Line-level dedup (preserves everything else including comments)
        let mut result_lines: Vec<String> = clean_content.lines().map(|l| l.to_string()).collect();
        if !lines_to_remove.is_empty() {
            result_lines.retain(|line| {
                let trimmed = line.trim();
                !lines_to_remove.iter().any(|dup| trimmed == dup)
            });
        }

        // 4. Build the new managed block from unified PassthroughBind list
        if !new_state.binds.is_empty() {
            let mut block = Vec::new();
            block.push("X-Lasper-Nvidia-Begin=managed-by-lasper".to_string());
            for bind in &new_state.binds {
                let host_path = escape_nspawn_bind_path(&bind.host_path);
                let container_path = escape_nspawn_bind_path(&bind.container_path);
                if bind.readonly {
                    if bind.host_path == bind.container_path {
                        block.push(format!("BindReadOnly={host_path}"));
                    } else {
                        block.push(format!("BindReadOnly={host_path}:{container_path}"));
                    }
                } else if bind.host_path == bind.container_path {
                    block.push(format!("Bind={host_path}"));
                } else {
                    block.push(format!("Bind={host_path}:{container_path}"));
                }
            }
            block.push("X-Lasper-Nvidia-End=true".to_string());

            // 5. Find [Files] section and insert block at its end
            let files_idx = result_lines
                .iter()
                .position(|l| l.trim().eq_ignore_ascii_case("[files]"));

            match files_idx {
                Some(idx) => {
                    let insert_at = result_lines
                        .iter()
                        .enumerate()
                        .skip(idx + 1)
                        .find(|(_, l)| l.trim().starts_with('[') && l.trim().ends_with(']'))
                        .map(|(i, _)| i)
                        .unwrap_or(result_lines.len());

                    for (i, line) in block.into_iter().enumerate() {
                        result_lines.insert(insert_at + i, line);
                    }
                }
                None => {
                    result_lines.push(String::new());
                    result_lines.push("[Files]".to_string());
                    result_lines.extend(block);
                }
            }
        }

        Ok(result_lines.join("\n"))
    }
}

//.nspawn file generation

/// Generate the content of a `.nspawn` container config file using AST.
pub fn nspawn_config_content(cfg: &ContainerConfig, xdg_runtime: Option<&str>) -> Result<String> {
    let spec = NspawnConfigSpec::try_from(cfg)?;
    nspawn_config_content_from_spec(&spec, xdg_runtime)
}

/// Generate `.nspawn` content from the privilege-safe configuration subset.
pub fn nspawn_config_content_from_spec(
    spec: &NspawnConfigSpec,
    xdg_runtime: Option<&str>,
) -> Result<String> {
    nspawn_config_content_from_spec_with_wayland_path(spec, xdg_runtime, None)
}

pub(crate) fn nspawn_config_content_from_spec_with_wayland_path(
    spec: &NspawnConfigSpec,
    xdg_runtime: Option<&str>,
    verified_wayland_socket: Option<&Path>,
) -> Result<String> {
    spec.validate()?;
    let mut conf = Ini::new();

    //[Exec]
    {
        let mut exec = conf.with_section(Some("Exec"));
        if spec.boot {
            exec.set("Boot", "yes");
        } else {
            exec.set("Boot", "no");
        }

        if let Some(mode) = spec.private_users {
            exec.set("PrivateUsers", mode.as_str());
        }

        if spec.privileged {
            exec.set("Capability", "all");
        }
        if !spec.hostname.is_empty() && spec.hostname != spec.machine.as_str() {
            exec.set("Hostname", &spec.hostname);
        }
    }

    //[Network]
    if let Some(mode) = &spec.network {
        use crate::nspawn::models::NetworkMode;
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
                for pf in &spec.port_forwards {
                    net.append(
                        "Port",
                        format!("{}:{}:{}", pf.protocol.as_str(), pf.host, pf.container),
                    );
                }
            }
            NetworkMode::Bridge(name) => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "yes")
                    .set("Bridge", name.clone());
                let net = conf.section_mut(Some("Network")).unwrap();
                for pf in &spec.port_forwards {
                    net.append(
                        "Port",
                        format!("{}:{}:{}", pf.protocol.as_str(), pf.host, pf.container),
                    );
                }
            }
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

    //[Files]
    let has_files = !spec.device_binds.is_empty()
        || !spec.readonly_binds.is_empty()
        || !spec.bind_mounts.is_empty()
        || spec.wayland_socket.is_some()
        || spec.graphics_acceleration
        || spec.nvidia_gpu
        || matches!(spec.network, Some(crate::nspawn::models::NetworkMode::Host));

    if has_files {
        conf.with_section(Some("Files")).set("__ensure_files", "");
        let files = conf.section_mut(Some("Files")).unwrap();
        files.remove("__ensure_files");
        if spec.nvidia_gpu {
            files.append("X-Lasper-Nvidia-Enabled", "true");
        }

        for dev in &spec.device_binds {
            files.append("Bind", dev.clone());
        }
        for ro in &spec.readonly_binds {
            files.append("BindReadOnly", ro.clone());
        }
        for bm in &spec.bind_mounts {
            if bm.readonly {
                files.append(
                    "BindReadOnly",
                    format!("{}:{}{}", bm.source, bm.target, bm.suffix),
                );
            } else {
                files.append("Bind", format!("{}:{}{}", bm.source, bm.target, bm.suffix));
            }
        }

        let suffix = if spec.private_users == Some(PrivateUsersMode::No) {
            ":noidmap"
        } else {
            ":idmap"
        };

        if matches!(spec.network, Some(crate::nspawn::models::NetworkMode::Host)) {
            files.append(
                "BindReadOnly",
                format!("/etc/resolv.conf:/etc/resolv.conf{}", suffix),
            );
        }

        if let Some(socket_name) = &spec.wayland_socket {
            let socket_path = verified_wayland_socket
                .map(PathBuf::from)
                .or_else(|| xdg_runtime.map(|runtime| PathBuf::from(runtime).join(socket_name)));
            if let Some(socket_path) = socket_path {
                let socket_path = validated_nspawn_path("Wayland socket path", &socket_path)?;
                files.append("Bind", format!("{socket_path}:/mnt/wayland-socket{suffix}"));
            }

            files.append(
                "BindReadOnly",
                format!("/tmp/.X11-unix:/mnt/host-x11{}", suffix),
            );

            if std::path::Path::new("/dev/dri").exists() {
                files.append("Bind", "/dev/dri");
            }
        }

        // Note: Individual device binds (/dev/dri, /dev/mali, etc.) are now
        // dynamically discovered and populated in cfg.device_binds by builder.rs.
    }

    let mut buffer = Vec::new();
    conf.write_to(&mut buffer)
        .map_err(|e| NspawnError::Runtime(format!("Failed to serialize INI: {}", e)))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn validated_nspawn_path<'a>(label: &str, path: &'a Path) -> Result<&'a str> {
    let path = path
        .to_str()
        .ok_or_else(|| NspawnError::Validation(format!("{label} is not valid UTF-8")))?;
    if path.chars().any(char::is_control) {
        return Err(NspawnError::Validation(format!(
            "{label} contains control characters"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{NetworkMode, PortForward};

    // Validation

    #[test]
    fn test_validate_machine_name_valid() {
        assert!(validate_machine_name("my-container").is_ok());
        assert!(validate_machine_name("test_01").is_ok());
        assert!(validate_machine_name("a.b").is_ok());
    }

    #[test]
    fn test_validate_machine_name_empty() {
        assert!(validate_machine_name("").is_err());
    }

    #[test]
    fn test_validate_machine_name_boundary_length() {
        assert!(validate_machine_name("a").is_ok());
        assert!(validate_machine_name(&"a".repeat(64)).is_ok());
        assert!(validate_machine_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn test_validate_machine_name_invalid_chars() {
        assert!(validate_machine_name("foo/bar").is_err());
        assert!(validate_machine_name("a b").is_err());
        assert!(validate_machine_name("rm -rf").is_err());
    }

    #[test]
    fn test_validate_machine_name_path_traversal() {
        assert!(validate_machine_name(".hidden").is_err());
        assert!(validate_machine_name("foo..bar").is_err());
        assert!(validate_machine_name("../../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_machine_name_injection_attacks() {
        assert!(validate_machine_name("foo\0bar").is_err());
        assert!(validate_machine_name("foo\nbar").is_err());
        assert!(validate_machine_name("foo;rm -rf /").is_err());
        assert!(validate_machine_name("$(whoami)").is_err());
    }

    // GPU enabled detection

    #[test]
    fn test_is_gpu_enabled_true() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nX-Lasper-Nvidia-Enabled=true".to_string(),
        };
        assert!(config.is_gpu_enabled());
    }

    #[test]
    fn test_is_gpu_enabled_false_value() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nX-Lasper-Nvidia-Enabled=false".to_string(),
        };
        assert!(!config.is_gpu_enabled());
    }

    #[test]
    fn test_is_gpu_enabled_missing_key() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nSomeOther=value".to_string(),
        };
        assert!(!config.is_gpu_enabled());
    }

    #[test]
    fn test_is_gpu_enabled_empty_content() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "".to_string(),
        };
        assert!(!config.is_gpu_enabled());
    }

    #[test]
    fn test_is_gpu_enabled_malformed_ini() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "not valid ini [[[[".to_string(),
        };
        assert!(!config.is_gpu_enabled());
    }

    // Purge nvidia block

    #[test]
    fn test_purge_nvidia_block_present() {
        let content = "Line 1\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nBindReadOnly=/usr/lib/libcuda.so\nX-Lasper-Nvidia-End=true\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, "Line 1\nLine 2");
        assert_eq!(death_list, vec!["/dev/nvidia0", "/usr/lib/libcuda.so"]);
    }

    #[test]
    fn test_purge_nvidia_block_absent() {
        let content = "Line 1\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, content);
        assert!(death_list.is_empty());
    }

    #[test]
    fn test_purge_nvidia_block_begin_only() {
        let content = "X-Lasper-Nvidia-Begin=managed-by-lasper\nLine 1";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_end_only() {
        let content = "Line 1\nX-Lasper-Nvidia-End=true\nLine 2";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_duplicate_begin() {
        let content = "X-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-End=true";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_reversed_markers() {
        let content =
            "X-Lasper-Nvidia-End=true\nBind=/dev/nvidia0\nX-Lasper-Nvidia-Begin=managed-by-lasper";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_empty_block() {
        let content =
            "Line 1\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-End=true\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, "Line 1\nLine 2");
        assert!(death_list.is_empty());
    }

    // Config content generation

    #[test]
    fn test_nspawn_config_content_minimal() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            boot: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("[Exec]"));
        assert!(content.contains("Boot=yes"));
    }

    #[test]
    fn test_nspawn_config_content_boot_disabled() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            boot: false,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Boot=no"));
    }

    #[test]
    fn test_nspawn_config_content_host_network() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Host),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("VirtualEthernet=no"));
        assert!(content.contains("BindReadOnly=/etc/resolv.conf:/etc/resolv.conf"));
    }

    #[test]
    fn test_nspawn_config_content_network_veth_with_ports() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Veth),
            port_forwards: vec![
                PortForward {
                    host: 8080,
                    container: 80,
                    proto: "tcp".to_string(),
                },
                PortForward {
                    host: 4443,
                    container: 443,
                    proto: "tcp".to_string(),
                },
            ],
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("VirtualEthernet=yes"));
        assert!(content.contains("Port=tcp:8080:80"));
        assert!(content.contains("Port=tcp:4443:443"));
    }

    #[test]
    fn test_nspawn_config_content_bridge_mode() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Bridge("br0".into())),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Bridge=br0"));
    }

    #[test]
    fn test_nspawn_config_content_privileged() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            privileged: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Capability=all"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_explicit_no() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            private_users: Some("no".into()),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("PrivateUsers=no"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_explicit_yes() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            private_users: Some("yes".into()),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("PrivateUsers=yes"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_pick() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            private_users: Some("pick".into()),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("PrivateUsers=pick"));
    }

    #[test]
    fn test_nspawn_config_content_nvidia_marker() {
        let cfg = ContainerConfig {
            name: "test".to_string(),
            nvidia_gpu: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("X-Lasper-Nvidia-Enabled=true"));
        assert!(content.contains("[Files]"));
        assert!(!content.contains("[General]"));
    }

    #[test]
    fn test_nspawn_config_content_rejects_invalid_name() {
        let cfg = ContainerConfig {
            name: "../escape".to_string(),
            ..Default::default()
        };
        assert!(nspawn_config_content(&cfg, None).is_err());
    }

    // GPU passthrough surgery

    #[test]
    fn test_apply_gpu_passthrough_to_content() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n".to_string();
        let new_state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/dri/by-path/nvidia-card".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/usr/lib/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("[Files]"));
        assert!(updated.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
        assert!(updated.contains("Bind=/dev/nvidia0:/dev/dri/by-path/nvidia-card"));
        assert!(updated.contains("BindReadOnly=/usr/lib/libcuda.so"));
        assert!(updated.contains("X-Lasper-Nvidia-End=true"));
    }

    #[test]
    fn nspawn_bind_codec_preserves_colons_inside_paths() {
        let path = "/dev/dri/by-path/pci-0000:01:00.0-card";
        assert_eq!(
            escape_nspawn_bind_path(path),
            r"/dev/dri/by-path/pci-0000\:01\:00.0-card"
        );
        assert_eq!(
            parse_nspawn_bind_paths(
                r"/dev/dri/card0:/dev/dri/by-path/pci-0000\:01\:00.0-card:noidmap"
            ),
            Some(("/dev/dri/card0".into(), path.into()))
        );
    }

    #[test]
    fn gpu_bind_escapes_pci_colons_in_container_destination() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/dri/card0".into(),
                container_path: "/dev/dri/by-path/pci-0000:01:00.0-card".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content("[Files]\n".into(), &state, &[])
                .unwrap();
        assert!(updated.contains(r"Bind=/dev/dri/card0:/dev/dri/by-path/pci-0000\:01\:00.0-card"));
    }

    #[test]
    fn test_apply_gpu_appends_to_existing_files_section() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/home/user:/home/user\n".to_string();
        let new_state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(
            updated.contains("Bind=/home/user:/home/user"),
            "User bind should survive"
        );
        assert!(updated.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
    }

    #[test]
    fn test_apply_gpu_preserves_comments() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n# My custom comment\n".to_string();
        let new_state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("# My custom comment"));
    }

    #[test]
    fn test_apply_gpu_dedup_legacy_binds() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/dev/nvidia0\n".to_string();
        let new_state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        let count = updated.matches("Bind=/dev/nvidia0").count();
        assert_eq!(
            count, 1,
            "Legacy duplicate should be removed, got:\n{}",
            updated
        );
    }

    #[test]
    fn test_apply_gpu_empty_state_is_noop() {
        let content = "[Exec]\nBoot=yes\n".to_string();
        let empty_state = crate::nspawn::platform::nvidia::NvidiaState::default();

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content.clone(), &empty_state, &[])
                .unwrap();
        assert!(!updated.contains("[Files]"));
        assert!(!updated.contains("X-Lasper-Nvidia-Begin"));
    }

    #[test]
    fn test_apply_gpu_with_symlink_as_bind() {
        use crate::nspawn::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n".to_string();
        let new_state = crate::nspawn::platform::nvidia::NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/host/libcuda.so.595.58.03".into(),
                    container_path: "/usr/lib/libcuda.so.1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so.595.58.03".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("BindReadOnly=/host/libcuda.so.595.58.03:/usr/lib/libcuda.so.1"));
        assert!(updated.contains("BindReadOnly=/host/libcuda.so.595.58.03:/usr/lib/libcuda.so"));
    }
}
