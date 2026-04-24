use super::discovery::get_nvidia_state;
use super::profile::{NvidiaPassthroughMode, NvidiaPassthroughProfile};
use super::state::{
    calculate_death_list, get_external_state, get_internal_state, save_external_state,
    save_internal_state, NvidiaState,
};
use crate::nspawn::errors::{NspawnError, Result};
use std::path::PathBuf;

macro_rules! log_step {
    ($name:expr, $step:expr, $msg:expr) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, $msg);
    };
    ($name:expr, $step:expr, $fmt:expr, $($arg:tt)*) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, format!($fmt, $($arg)*));
    };
}

pub async fn cleanup_container_garbage(name: &str, death_list: &[String]) -> Result<()> {
    log_step!(
        name,
        "Cleanup",
        "Inspecting and removing 0-byte driver files from host..."
    );

    // 1. Mount rootfs
    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;

    // 2. Precise cleanup: Iterate and remove 0-byte files
    for path in death_list {
        let target = rootfs.join(path.trim_start_matches('/'));
        if tokio::fs::try_exists(&target).await.unwrap_or(false) {
            if let Ok(meta) = tokio::fs::metadata(&target).await {
                if meta.len() == 0 {
                    log_step!(name, "Cleanup", "Deleting 0-byte junk: {}", path);
                    let _ = tokio::fs::remove_file(&target).await;
                }
            }
        }
    }

    // 3. Unmount (with retry to prevent loop device leaks)
    if let Err(e) = backend.unmount(name).await {
        log::warn!(
            "[AUDIT] [Container: {}] [Step: Cleanup] Unmount failed: {}. Retrying...",
            name,
            e
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if let Err(e2) = backend.unmount(name).await {
            log::error!(
                "[AUDIT] [Container: {}] [Step: Cleanup] Unmount retry failed: {}. Loopback device may be leaked.",
                name,
                e2
            );
        }
    }

    Ok(())
}

async fn inject_persistent_device_allow(name: &str, state: &NvidiaState) -> Result<()> {
    let dir = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        name
    ));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| NspawnError::Io(dir.clone(), e))?;

    let path = dir.join("10-lasper-nvidia.conf");
    let mut content = String::from("[Service]\n");
    for dev in &state.device_binds {
        content.push_str(&format!("DeviceAllow={} rw\n", dev));
    }

    crate::nspawn::sys::io::AsyncLockedWriter::write_atomic(&path, &content).await?;

    // Cleanup old transient one if present
    let transient_path = format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d/10-lasper-nvidia.conf",
        name
    );
    let _ = tokio::fs::remove_file(transient_path).await;

    Ok(())
}

pub fn apply_category_remapping(host_state: &mut NvidiaState, profile: &NvidiaPassthroughProfile) {
    if profile.mode != NvidiaPassthroughMode::Categorized {
        return;
    }

    // Build dir_remap from well-known category roots → user destinations.
    let mut dir_remap: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (cat, dest) in &profile.category_destinations {
        let root = cat.default_container_root();
        if !root.is_empty() {
            dir_remap.insert(root.to_string(), dest.trim_end_matches('/').to_string());
        }
    }

    // Remap classified_entries: preserve subdirectory structure below root
    for entry in &mut host_state.classified_entries {
        if let Some(dest_dir) = profile.category_destinations.get(&entry.category) {
            let root = entry.category.default_container_root();
            let dest = dest_dir.trim_end_matches('/');
            if !root.is_empty() && entry.default_container_path.starts_with(root) {
                let relative = &entry.default_container_path[root.len()..];
                entry.default_container_path = format!("{}{}", dest, relative);
            } else if root.is_empty() {
                // No canonical root (Config) — keep CDI's original container path
            } else {
                let filename = entry.default_container_path.split('/').last().unwrap_or_default();
                entry.default_container_path = format!("{}/{}", dest, filename);
            }
        }
    }

    // Helper: remap a path using prefix matching against dir_remap
    let remap_path = |path: &str, dir_remap: &std::collections::HashMap<String, String>| -> Option<String> {
        let mut best_root = "";
        let mut best_dest = "";
        for (root, dest) in dir_remap {
            if path.starts_with(root.as_str()) && root.len() > best_root.len() {
                best_root = root;
                best_dest = dest;
            }
        }
        if !best_root.is_empty() {
            let relative = &path[best_root.len()..];
            Some(format!("{}{}", best_dest, relative))
        } else {
            None
        }
    };

    // Remap symlinks
    for sym in &mut host_state.symlinks {
        if let Some(new_path) = remap_path(&sym.link_path, &dir_remap) {
            sym.link_path = new_path;
        }
        if sym.target.starts_with('/') {
            if let Some(new_path) = remap_path(&sym.target, &dir_remap) {
                sym.target = new_path;
            }
        }
    }

    // Remap readonly_binds (symlink aliases from resolve_so_aliases)
    for ro in &mut host_state.readonly_binds {
        let (host_part, container_part) = if let Some((h, c)) = ro.split_once(':') {
            (h.to_string(), c.to_string())
        } else {
            (ro.clone(), ro.clone())
        };

        if let Some(new_container_path) = remap_path(&container_part, &dir_remap) {
            *ro = format!("{}:{}", host_part, new_container_path);
        } else {
            *ro = format!("{}:{}", host_part, container_part);
        }
    }
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    dbus: &crate::nspawn::adapters::comm::dbus::DbusProvider,
) -> Result<()> {
    // 1. Semantic Marker Check
    let config = match crate::nspawn::adapters::config::nspawn_file::NspawnConfig::load(name).await
    {
        Some(c) => c,
        None => return Ok(()),
    };
    if !config.is_gpu_enabled() {
        return Ok(());
    }

    log_step!(
        name,
        "Lifecycle",
        "GPU Passthrough enabled, initiating state synchronization..."
    );

    // 2. State Diff Engine (Declarative)
    log_step!(name, "Detection", "Scanning host for NVIDIA CDI devices...");
    let profile = NvidiaPassthroughProfile::load(name).await.unwrap_or_default();
    let mut host_state = get_nvidia_state(Some(&profile)).await?;

    // Apply remapping if in Categorized mode
    if profile.mode == NvidiaPassthroughMode::Categorized {
        log_step!(name, "Remapping", "Applying custom destination remapping...");
        apply_category_remapping(&mut host_state, &profile);
    }

    log_step!(
        name,
        "Detection",
        "Detected driver: {}, {} libraries, {} devices.",
        host_state.driver_version,
        host_state.readonly_binds.len(),
        host_state.device_binds.len()
    );

    let external_cache = get_external_state(name).await?.unwrap_or_default();

    // Full-payload comparison for perfect state sync
    let mut old_state = external_cache.clone();
    if external_cache != host_state && !external_cache.driver_version.is_empty() {
        if let Ok(Some(internal)) = get_internal_state(name).await {
            old_state = internal;
        }
    }

    if old_state == host_state && !old_state.driver_version.is_empty() {
        log::debug!(
            "GPU state identity match for {}, skipping re-assembly.",
            name
        );
        // We still inject persistent to be safe
        inject_persistent_device_allow(name, &host_state).await?;
        let _ = dbus.reload_daemon().await;
        return Ok(());
    }

    log::info!(
        "GPU driver change detected ({} -> {}), performing surgery...",
        old_state.driver_version,
        host_state.driver_version
    );

    let death_list = calculate_death_list(&old_state, &host_state);
    if !death_list.is_empty() {
        log_step!(
            name,
            "Surgery",
            "Marked {} files for removal/update.",
            death_list.len()
        );
    }

    // 3. Physical Cleanup
    cleanup_container_garbage(name, &death_list).await?;

    // 4. AST mutation
    log_step!(name, "Surgery", "Mutating .nspawn configuration AST...");
    crate::nspawn::adapters::config::nspawn_file::NspawnConfig::update_gpu_passthrough(
        name,
        &host_state,
        &death_list,
    )
    .await?;

    // 5. Persistent Injection & Dual-Track Sync
    log_step!(
        name,
        "Surgery",
        "Persisting state and injecting persistent DeviceAllow rules..."
    );
    save_external_state(name, &host_state).await?;
    save_internal_state(name, &host_state).await?;
    inject_persistent_device_allow(name, &host_state).await?;

    // 6. Hook execution (Symlinks, Env)
    log_step!(name, "Surgery", "Creating symlinks and injecting environment...");
    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;

    // Symlinks
    for sym in &host_state.symlinks {
        let target = rootfs.join(sym.link_path.trim_start_matches('/'));
        if let Some(parent) = target.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::remove_file(&target).await;
        if let Err(e) = std::os::unix::fs::symlink(&sym.target, &target) {
            log::warn!("Failed to create symlink {} -> {}: {}", target.display(), sym.target, e);
        }
    }

    // Environment
    if profile.inject_env {
        let env_path = rootfs.join("etc/environment");
        if let Ok(content) = tokio::fs::read_to_string(&env_path).await {
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            for (key, val) in &host_state.env_vars {
                let prefix = format!("{}=", key);
                lines.retain(|l| !l.starts_with(&prefix));
                lines.push(format!("{}={}", key, val));
            }
            let _ = tokio::fs::write(&env_path, lines.join("\n") + "\n").await;
        }
        
        // ldconfig
        let ld_conf_dir = rootfs.join("etc/ld.so.conf.d");
        let _ = tokio::fs::create_dir_all(&ld_conf_dir).await;
        let ld_conf_path = ld_conf_dir.join("lasper-nvidia.conf");
        let mut ld_content = String::new();
        for folder in &host_state.ldcache_folders {
            ld_content.push_str(folder);
            ld_content.push('\n');
        }
        // Also add remapped lib dirs
        if profile.mode == NvidiaPassthroughMode::Categorized {
            use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
            for cat in [NvidiaFileCategory::Lib64, NvidiaFileCategory::Lib32] {
                if let Some(dest) = profile.category_destinations.get(&cat) {
                    ld_content.push_str(dest);
                    ld_content.push('\n');
                }
            }
        }
        let _ = tokio::fs::write(&ld_conf_path, ld_content).await;

        // Run ldconfig inside the container rootfs to rebuild the cache
        let _ = std::process::Command::new("chroot")
            .arg(rootfs.to_string_lossy().as_ref())
            .arg("ldconfig")
            .output();
    }

    let _ = backend.unmount(name).await;

    // 7. Zero-overhead Reload
    log_step!(
        name,
        "Lifecycle",
        "Reloading systemd daemon to commit changes."
    );
    dbus.reload_daemon().await?;

    log_step!(name, "Lifecycle", "GPU surgery successful.");
    Ok(())
}
