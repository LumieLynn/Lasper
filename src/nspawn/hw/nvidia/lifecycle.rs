use crate::nspawn::errors::{NspawnError, Result};
use std::path::PathBuf;
use super::state::{NvidiaState, get_external_state, get_internal_state, save_external_state, save_internal_state, calculate_death_list};
use super::discovery::get_nvidia_state;

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
    let backend = crate::nspawn::storage::get_storage_backend_for(name).await;
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

    // 3. Unmount
    let _ = backend.unmount(name).await;

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

    crate::nspawn::utils::io::AsyncLockedWriter::write_atomic(&path, &content).await?;

    // Cleanup old transient one if present
    let transient_path = format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d/10-lasper-nvidia.conf",
        name
    );
    let _ = tokio::fs::remove_file(transient_path).await;

    Ok(())
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    dbus: &crate::nspawn::core::provider::dbus::DbusProvider,
) -> Result<()> {
    // 1. Semantic Marker Check
    let config = match crate::nspawn::config::nspawn_file::NspawnConfig::load(name).await {
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
    let host_state = get_nvidia_state().await?;
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
    crate::nspawn::config::nspawn_file::NspawnConfig::update_gpu_passthrough(name, &host_state, &death_list).await?;

    // 5. Persistent Injection & Dual-Track Sync
    log_step!(
        name,
        "Surgery",
        "Persisting state and injecting persistent DeviceAllow rules..."
    );
    save_external_state(name, &host_state).await?;
    save_internal_state(name, &host_state).await?;
    inject_persistent_device_allow(name, &host_state).await?;

    // 6. Zero-overhead Reload
    log_step!(
        name,
        "Lifecycle",
        "Reloading systemd daemon to commit changes."
    );
    dbus.reload_daemon().await?;

    log_step!(name, "Lifecycle", "GPU surgery successful.");
    Ok(())
}
