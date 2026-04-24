use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use ini::Ini;
use std::path::{Path, PathBuf};

/// Raw content of a `.nspawn` config file from `/etc/systemd/nspawn/`.
pub struct NspawnConfig {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub content: String,
}

/// Validates a container name matches systemd machine name constraints.
/// Defense-in-depth: the wizard UI already validates this, but backend
/// must not trust inputs blindly in case of restricted-sudo environments.
pub fn validate_machine_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || name.starts_with('.')
        || name.contains("..")
    {
        return Err(NspawnError::Validation(format!(
            "Invalid machine name: '{}'. Must be 1-64 chars, [a-zA-Z0-9_.-], no leading dot or '..'",
            name
        )));
    }
    Ok(())
}

impl NspawnConfig {
    pub fn default_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name))
    }

    /// Load the `.nspawn` config for a container by name.
    pub async fn load(name: &str) -> Option<NspawnConfig> {
        if validate_machine_name(name).is_err() {
            return None;
        }
        let path = Self::default_path(name);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Some(NspawnConfig { path, content }),
            Err(e) => {
                log::debug!("Could not read .nspawn config for {}: {}", name, e);
                Option::None
            }
        }
    }

    /// Check if the NVIDIA GPU passthrough is enabled for this container.
    pub fn is_gpu_enabled(&self) -> bool {
        let conf = match Ini::load_from_str(&self.content) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // Check both [General] and the global (None) section for compatibility
        let enabled_msg = "X-Lasper-Nvidia-Enabled";
        let in_general = conf.get_from(Some("General"), enabled_msg);
        let in_global = conf.get_from(None::<&str>, enabled_msg);

        in_general
            .or(in_global)
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false)
    }

    /// Update the .nspawn config using precision AST mutation.
    pub async fn update_gpu_passthrough(
        name: &str,
        new_state: &crate::nspawn::platform::nvidia::NvidiaState,
        death_list: &[String],
    ) -> Result<()> {
        validate_machine_name(name)?;
        let path = Self::default_path(name);

        crate::nspawn::sys::io::AsyncLockedWriter::write_locked(&path, |existing| {
            let content = existing.ok_or_else(|| {
                NspawnError::Io(
                    path.clone(),
                    std::io::Error::new(std::io::ErrorKind::NotFound, "Config file not found"),
                )
            })?;

            Self::apply_gpu_passthrough_to_content(content, new_state, death_list)
        })
        .await
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
                for i in s + 1..e {
                    let line = lines[i].trim();
                    if line.starts_with("Bind=") || line.starts_with("BindReadOnly=") {
                        if let Some(val) = line.splitn(2, '=').nth(1) {
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
        _death_list: &[String], // No longer strictly needed for config update if using markers
    ) -> Result<String> {
        // 1. Purge existing block using markers (preserves everything else)
        let (clean_content, _extracted_deaths) = Self::purge_nvidia_block(&content)?;

        // 2. Read-only INI parse for legacy dedup detection
        let mut lines_to_remove: Vec<String> = Vec::new();
        if let Ok(conf) = Ini::load_from_str(&clean_content) {
            if let Some(files_section) = conf.section(Some("Files")) {
                for (key, value) in files_section.iter() {
                    if key == "Bind" && new_state.device_binds.iter().any(|v| v == value) {
                        lines_to_remove.push(format!("Bind={}", value));
                    }
                    if key == "BindReadOnly" {
                        let host_path = value.split(':').next().unwrap_or(value);
                        let is_in_ro = new_state
                            .readonly_binds
                            .iter()
                            .any(|v| v.split(':').next().unwrap_or(v) == host_path);
                        let is_in_ce = new_state
                            .classified_entries
                            .iter()
                            .any(|ce| ce.host_path == host_path);
                        if is_in_ro || is_in_ce {
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

        // 4. Build the new managed block
        if !new_state.device_binds.is_empty()
            || !new_state.readonly_binds.is_empty()
            || !new_state.classified_entries.is_empty()
        {
            let mut block = Vec::new();
            block.push("X-Lasper-Nvidia-Begin=managed-by-lasper".to_string());
            for dev in &new_state.device_binds {
                block.push(format!("Bind={}", dev));
            }
            for ro in &new_state.readonly_binds {
                block.push(format!("BindReadOnly={}", ro));
            }
            for ce in &new_state.classified_entries {
                if ce.host_path == ce.default_container_path {
                    block.push(format!("BindReadOnly={}", ce.host_path));
                } else {
                    block.push(format!(
                        "BindReadOnly={}:{}",
                        ce.host_path, ce.default_container_path
                    ));
                }
            }
            block.push("X-Lasper-Nvidia-End=true".to_string());

            // 5. Find [Files] section and insert block at its end
            let files_idx = result_lines
                .iter()
                .position(|l| l.trim().eq_ignore_ascii_case("[files]"));

            match files_idx {
                Some(idx) => {
                    // Find end of [Files] section: next section header or EOF
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
                    // No [Files] section exists — append one
                    result_lines.push(String::new());
                    result_lines.push("[Files]".to_string());
                    result_lines.extend(block);
                }
            }
        }

        Ok(result_lines.join("\n"))
    }
}

// ── .nspawn file generation ───────────────────────────────────────────────────

/// Generate the content of a `.nspawn` container config file using AST.
pub fn nspawn_config_content(cfg: &ContainerConfig, xdg_runtime: Option<&str>) -> Result<String> {
    validate_machine_name(&cfg.name)?;
    let mut conf = Ini::new();
    let idmap_supported = crate::nspawn::platform::capabilities::supports_idmap();

    if cfg.nvidia_gpu {
        conf.with_section(Some("General"))
            .set("X-Lasper-Nvidia-Enabled", "true");
    }

    // ── [Exec] ────────────────────────────────────────────────────────────────
    {
        let mut exec = conf.with_section(Some("Exec"));
        if cfg.boot {
            exec.set("Boot", "yes");
        } else {
            exec.set("Boot", "no");
        }

        // If idmap is NOT supported, we MUST disable PrivateUsers (the security compromise)
        // DRI and Wayland sockets typically don't work in a namespaced environment without it.
        if !idmap_supported
            && (cfg.wayland_socket.is_some() || cfg.graphics_acceleration || cfg.privileged)
        {
            exec.set("PrivateUsers", "no");
        }

        if cfg.privileged {
            exec.set("Capability", "all");
        }
        if !cfg.hostname.is_empty() && cfg.hostname != cfg.name {
            exec.set("Hostname", &cfg.hostname);
        }
    }

    // ── [Network] ─────────────────────────────────────────────────────────────
    if let Some(mode) = &cfg.network {
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
        || cfg.wayland_socket.is_some()
        || cfg.graphics_acceleration;

    if has_files {
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

        let suffix = if idmap_supported { ":idmap" } else { "" };

        if let Some(socket_name) = &cfg.wayland_socket {
            if let Some(runtime) = xdg_runtime {
                let socket_path = std::path::PathBuf::from(runtime).join(socket_name);
                files.append(
                    "Bind",
                    format!("{}:/mnt/wayland-socket{}", socket_path.display(), suffix),
                );
            }

            files.append("Bind", format!("/tmp/.X11-unix:/tmp/.X11-unix{}", suffix));

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

/// Clones an .nspawn configuration file from one container to another.
pub async fn clone_nspawn_config(source_name: &str, dest_name: &str) -> Result<()> {
    validate_machine_name(source_name)?;
    validate_machine_name(dest_name)?;
    let source_path = NspawnConfig::default_path(source_name);
    if !tokio::fs::try_exists(&source_path).await.unwrap_or(false) {
        return Ok(());
    }
    let dest_path = NspawnConfig::default_path(dest_name);

    if let Some(parent) = Path::new(&dest_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
    }

    crate::nspawn::sys::io::AsyncLockedWriter::atomic_copy(
        Path::new(&source_path),
        Path::new(&dest_path),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{NetworkMode, PortForward};

    // ── Validation ────────────────────────────────────────────────────────

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

    // ── GPU enabled detection ─────────────────────────────────────────────

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

    // ── Purge nvidia block ────────────────────────────────────────────────

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

    // ── Config content generation ─────────────────────────────────────────

    #[test]
    fn test_nspawn_config_content_minimal() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.boot = true;
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("[Exec]"));
        assert!(content.contains("Boot=yes"));
    }

    #[test]
    fn test_nspawn_config_content_boot_disabled() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.boot = false;
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Boot=no"));
    }

    #[test]
    fn test_nspawn_config_content_host_network() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.network = Some(NetworkMode::Host);
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("VirtualEthernet=no"));
    }

    #[test]
    fn test_nspawn_config_content_network_veth_with_ports() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.network = Some(NetworkMode::Veth);
        cfg.port_forwards = vec![
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
        ];
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("VirtualEthernet=yes"));
        assert!(content.contains("Port=tcp:8080:80"));
        assert!(content.contains("Port=tcp:4443:443"));
    }

    #[test]
    fn test_nspawn_config_content_bridge_mode() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.network = Some(NetworkMode::Bridge("br0".into()));
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Bridge=br0"));
    }

    #[test]
    fn test_nspawn_config_content_privileged() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.privileged = true;
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("Capability=all"));
    }

    #[test]
    fn test_nspawn_config_content_nvidia_marker() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "test".to_string();
        cfg.nvidia_gpu = true;
        let content = nspawn_config_content(&cfg, None).unwrap();
        assert!(content.contains("X-Lasper-Nvidia-Enabled=true"));
    }

    #[test]
    fn test_nspawn_config_content_rejects_invalid_name() {
        let mut cfg = ContainerConfig::default();
        cfg.name = "../escape".to_string();
        assert!(nspawn_config_content(&cfg, None).is_err());
    }

    // ── GPU passthrough surgery ───────────────────────────────────────────

    #[test]
    fn test_apply_gpu_passthrough_to_content() {
        let content = "[Exec]\nBoot=yes\n".to_string();
        let mut new_state = crate::nspawn::platform::nvidia::NvidiaState::default();
        new_state.device_binds = vec!["/dev/nvidia0".to_string()];
        new_state.readonly_binds = vec!["/usr/lib/libcuda.so".to_string()];

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("[Files]"));
        assert!(updated.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
        assert!(updated.contains("Bind=/dev/nvidia0"));
        assert!(updated.contains("BindReadOnly=/usr/lib/libcuda.so"));
        assert!(updated.contains("X-Lasper-Nvidia-End=true"));
    }

    #[test]
    fn test_apply_gpu_appends_to_existing_files_section() {
        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/home/user:/home/user\n".to_string();
        let mut new_state = crate::nspawn::platform::nvidia::NvidiaState::default();
        new_state.device_binds = vec!["/dev/nvidia0".to_string()];

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
        let content = "[Exec]\nBoot=yes\n# My custom comment\n".to_string();
        let mut new_state = crate::nspawn::platform::nvidia::NvidiaState::default();
        new_state.device_binds = vec!["/dev/nvidia0".to_string()];

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("# My custom comment"));
    }

    #[test]
    fn test_apply_gpu_dedup_legacy_binds() {
        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/dev/nvidia0\n".to_string();
        let mut new_state = crate::nspawn::platform::nvidia::NvidiaState::default();
        new_state.device_binds = vec!["/dev/nvidia0".to_string()];

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
}
