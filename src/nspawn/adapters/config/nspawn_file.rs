use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use ini::Ini;
use std::path::{Path, PathBuf};

/// Raw content of a `.nspawn` config file from `/etc/systemd/nspawn/`.
pub struct NspawnConfig {
    pub path: PathBuf,
    pub content: String,
}

/// Validates a container name matches systemd machine name constraints.
/// Defense-in-depth: the wizard UI already validates this, but backend
/// must not trust inputs blindly in case of restricted-sudo environments.
pub fn validate_machine_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
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
    /// Load the `.nspawn` config for a container by name.
    pub async fn load(name: &str) -> Option<NspawnConfig> {
        if validate_machine_name(name).is_err() {
            return None;
        }
        let path = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        match tokio::fs::read_to_string(&path)
            .await
        {
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
        let path = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));

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
                    if key == "BindReadOnly" && new_state.readonly_binds.iter().any(|v| v == value) {
                        lines_to_remove.push(format!("BindReadOnly={}", value));
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
        if !new_state.device_binds.is_empty() || !new_state.readonly_binds.is_empty() {
            let mut block = Vec::new();
            block.push("X-Lasper-Nvidia-Begin=managed-by-lasper".to_string());
            for dev in &new_state.device_binds {
                block.push(format!("Bind={}", dev));
            }
            for ro in &new_state.readonly_binds {
                block.push(format!("BindReadOnly={}", ro));
            }
            block.push("X-Lasper-Nvidia-End=true".to_string());

            // 5. Find [Files] section and insert block at its end
            let files_idx = result_lines.iter().position(|l| {
                l.trim().eq_ignore_ascii_case("[files]")
            });

            match files_idx {
                Some(idx) => {
                    // Find end of [Files] section: next section header or EOF
                    let insert_at = result_lines.iter()
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
                files.append("Bind", format!("{}:/run/wayland-0", socket_path.display()));
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
    let source_path = format!("/etc/systemd/nspawn/{}.nspawn", source_name);
    if !tokio::fs::try_exists(&source_path)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    let dest_path = format!("/etc/systemd/nspawn/{}.nspawn", dest_name);

    if let Some(parent) = Path::new(&dest_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
    }

    crate::nspawn::sys::io::AsyncLockedWriter::atomic_copy(Path::new(&source_path), Path::new(&dest_path)).await?;
    Ok(())
}
