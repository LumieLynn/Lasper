use super::discovery::get_nvidia_state;
use super::profile::NvidiaPassthroughMode;
use super::state::{calculate_death_list, get_external_state, save_external_state, NvidiaState};
use crate::nspawn::errors::{NspawnError, Result};
use std::path::PathBuf;

macro_rules! log_step {
    ($name:expr, $step:expr, $msg:expr) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, $msg);
    };
    ($name:expr, $step:expr, $fmt:expr, $($arg:tt)*) => {
        log::info!(
            "[AUDIT] [Container: {}] [Step: {}] {}",
            $name,
            $step,
            format!($fmt, $($arg)*)
        );
    };
}

pub async fn cleanup_container_garbage(name: &str, death_list: &[String]) -> Result<()> {
    log_step!(
        name,
        "Cleanup",
        "Inspecting and removing leftover driver files..."
    );

    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;

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

    if let Err(e) = backend.unmount(name).await {
        log::warn!(
            "[AUDIT] [Container: {}] [Step: Cleanup] Unmount failed: {}. Retrying...",
            name,
            e
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if let Err(e2) = backend.unmount(name).await {
            log::error!(
                "[AUDIT] [Container: {}] [Step: Cleanup] Unmount retry failed: {}",
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
    for bind in &state.binds {
        if !bind.readonly {
            content.push_str(&format!("DeviceAllow={} rw\n", bind.host_path));
        }
    }

    crate::nspawn::sys::io::AsyncLockedWriter::write_atomic(&path, &content).await?;

    let transient_path = format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d/10-lasper-nvidia.conf",
        name
    );
    let _ = tokio::fs::remove_file(transient_path).await;

    Ok(())
}

/// Write ld.so.conf.d entry and /etc/environment vars into the container rootfs.
/// Done at creation time (not every startup) — called from provisioning path.
pub async fn inject_env_once(name: &str, state: &NvidiaState) -> Result<()> {
    let backend = crate::nspawn::adapters::storage::get_storage_backend_for(name).await;
    let rootfs = backend.mount(name).await?;

    // ld.so.conf.d
    let ld_conf_dir = rootfs.join("etc/ld.so.conf.d");
    let _ = tokio::fs::create_dir_all(&ld_conf_dir).await;
    let ld_conf_path = ld_conf_dir.join("lasper-nvidia.conf");
    let mut ld_content = String::new();
    for folder in &state.ldcache_folders {
        ld_content.push_str(folder);
        ld_content.push('\n');
    }
    // Add remapped lib dirs
    if let Some(ref prof) = state.profile {
        if prof.mode == NvidiaPassthroughMode::Categorized {
            use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
            for cat in [NvidiaFileCategory::Lib64, NvidiaFileCategory::Lib32] {
                if let Some(dest) = prof.category_destinations.get(&cat) {
                    ld_content.push_str(dest);
                    ld_content.push('\n');
                }
            }
        }
    }
    let _ = tokio::fs::write(&ld_conf_path, ld_content).await;

    // /etc/environment
    if state.profile.as_ref().is_some_and(|p| p.inject_env) {
        let env_path = rootfs.join("etc/environment");
        if let Ok(content) = tokio::fs::read_to_string(&env_path).await {
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            for (key, val) in &state.env_vars {
                let prefix = format!("{}=", key);
                lines.retain(|l| !l.starts_with(&prefix));
                lines.push(format!("{}={}", key, val));
            }
            let _ = tokio::fs::write(&env_path, lines.join("\n") + "\n").await;
        }
    }

    let _ = backend.unmount(name).await;
    Ok(())
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    dbus: &dyn crate::nspawn::adapters::comm::dbus::DbusProvider,
) -> Result<()> {
    // 1. Check if GPU passthrough is enabled in .nspawn config
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

    // 2. Load old state and profile, then scan host
    log_step!(name, "Detection", "Scanning host for NVIDIA CDI devices...");

    let external_cache = get_external_state(name).await?.unwrap_or_default();
    let profile = external_cache.profile.clone().unwrap_or_default();

    // Remapping already happens inside get_nvidia_state
    let host_state = get_nvidia_state(Some(&profile)).await?;

    log_step!(
        name,
        "Detection",
        "Detected driver: {}, {} binds, {} ldconfig folders.",
        host_state.driver_version,
        host_state.binds.len(),
        host_state.ldcache_folders.len()
    );

    // 3. Compare old vs new state
    let old_state = external_cache.clone();

    if old_state == host_state && !old_state.driver_version.is_empty() {
        // State matches — verify .nspawn markers exist as a sanity check
        if config
            .content
            .contains("X-Lasper-Nvidia-Begin=managed-by-lasper")
        {
            log::debug!(
                "GPU state identity match for {}, skipping re-assembly.",
                name
            );
            inject_persistent_device_allow(name, &host_state).await?;
            let _ = dbus.reload_daemon().await;
            return Ok(());
        }
        log::info!(
            "GPU state matches but .nspawn markers missing for {} — regenerating.",
            name
        );
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

    // 4. Cleanup stale files in rootfs
    cleanup_container_garbage(name, &death_list).await?;

    // 5. Update .nspawn config (symlinks are now synthesized as Bind entries here)
    log_step!(name, "Surgery", "Mutating .nspawn configuration AST...");
    crate::nspawn::adapters::config::nspawn_file::NspawnConfig::update_gpu_passthrough(
        name,
        &host_state,
        &death_list,
    )
    .await?;

    // 6. Persist state and inject DeviceAllow rules
    log_step!(
        name,
        "Surgery",
        "Persisting state and injecting persistent DeviceAllow rules..."
    );
    save_external_state(name, &host_state).await?;
    inject_persistent_device_allow(name, &host_state).await?;

    // 7. Reload daemon
    log_step!(
        name,
        "Lifecycle",
        "Reloading systemd daemon to commit changes."
    );
    dbus.reload_daemon().await?;

    log_step!(name, "Lifecycle", "GPU surgery successful.");
    Ok(())
}
