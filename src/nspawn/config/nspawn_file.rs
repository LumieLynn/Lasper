use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use ini::Ini;
use std::path::{Path, PathBuf};

/// Raw content of a `.nspawn` config file from `/etc/systemd/nspawn/`.
pub struct NspawnConfig {
    pub path: PathBuf,
    pub content: String,
}

impl NspawnConfig {
    /// Load the `.nspawn` config for a container by name.
    pub fn load(name: &str) -> Option<NspawnConfig> {
        let path = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        match std::fs::read_to_string(&path) {
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
        new_state: &crate::nspawn::hw::nvidia::NvidiaState,
        death_list: &[String],
    ) -> Result<()> {
        let path = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        let content =
            std::fs::read_to_string(&path).map_err(|e| NspawnError::Io(path.clone(), e))?;

        let final_content = Self::apply_gpu_passthrough_to_content(content, new_state, death_list)?;

        // Atomic write
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
        }
        let tmp_path = path.with_extension("nspawn.tmp");
        std::fs::write(&tmp_path, final_content)
            .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;

        tokio::fs::rename(&tmp_path, &path)
            .await
            .map_err(|e| NspawnError::Io(path, e))?;

        Ok(())
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
        new_state: &crate::nspawn::hw::nvidia::NvidiaState,
        _death_list: &[String], // No longer strictly needed for config update if using markers
    ) -> Result<String> {
        // 1. Purge existing block using markers
        let (clean_content, _extracted_deaths) = Self::purge_nvidia_block(&content)?;

        let original_conf = Ini::load_from_str(&clean_content)
            .map_err(|e| NspawnError::Runtime(format!("Failed to parse .nspawn as INI: {}", e)))?;

        // 2. Rebuild Ini to ensure clean section structure
        let mut new_conf = Ini::new();
        for (section_name, props) in original_conf.iter() {
            let section_str = section_name.as_deref();
            for (key, value) in props.iter() {
                // FALLBACK DE-DUPLICATION:
                // If this is a Bind entry that is ALREADY in our new managed state,
                // skip it here so it doesn't get written twice (it will be added in markers later).
                // This handles the transition from unmarked legacy files to marked ones.
                if section_str == Some("Files") {
                    if key == "Bind" && new_state.device_binds.iter().any(|v| v == value) {
                        continue;
                    }
                    if key == "BindReadOnly" && new_state.readonly_binds.iter().any(|v| v == value)
                    {
                        continue;
                    }
                }

                if new_conf.section(section_str).is_none() {
                    new_conf
                        .with_section(section_str)
                        .set(key.to_string(), value.to_string());
                } else {
                    new_conf
                        .section_mut(section_str)
                        .unwrap()
                        .append(key.to_string(), value.to_string());
                }
            }
        }

        // 3. Add New Managed Block to [Files]
        if !new_state.device_binds.is_empty() || !new_state.readonly_binds.is_empty() {
            // Ensure [Files] section exists
            if new_conf.section(Some("Files")).is_none() {
                new_conf
                    .with_section(Some("Files"))
                    .set("__placeholder", "");
                new_conf
                    .section_mut(Some("Files"))
                    .unwrap()
                    .remove("__placeholder");
            }
            let s = new_conf.section_mut(Some("Files")).unwrap();

            // Markers (Phase 3: Static semantic tagging)
            s.append("X-Lasper-Nvidia-Begin", "managed-by-lasper");
            for dev in &new_state.device_binds {
                s.append("Bind", dev.clone());
            }
            for ro in &new_state.readonly_binds {
                s.append("BindReadOnly", ro.clone());
            }
            s.append("X-Lasper-Nvidia-End", "true");
        }

        // 4. Serialize back to buffer
        let mut buffer = Vec::new();
        new_conf
            .write_to(&mut buffer)
            .map_err(|e| NspawnError::Runtime(format!("Failed to serialize INI: {}", e)))?;

        Ok(String::from_utf8_lossy(&buffer).into_owned())
    }
}

// ── .nspawn file generation ───────────────────────────────────────────────────

/// Generate the content of a `.nspawn` container config file using AST.
pub fn nspawn_config_content(cfg: &ContainerConfig) -> Result<String> {
    let mut conf = Ini::new();
    let idmap_supported = crate::nspawn::utils::discovery::supports_idmap();

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
            let xdg_runtime = crate::nspawn::utils::discovery::get_xdg_runtime()?;
            let host_wayland_sock = format!("{}/{}", xdg_runtime, socket_name);

            files.append(
                "Bind",
                format!("{}:/mnt/wayland-socket{}", host_wayland_sock, suffix),
            );
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
