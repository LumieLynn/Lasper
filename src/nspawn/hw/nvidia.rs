//! NVIDIA GPU and driver detection logic for host passthrough.

use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use crate::nspawn::utils::new_command;

/// Hardware and driver information detected on the host for mounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NvidiaState {
    /// Host driver version when this state was captured.
    pub driver_version: String,
    /// Host paths to bind-mount into the container (read-only).
    pub readonly_binds: Vec<String>,
    /// Device files to bind-mount (read-write).
    pub device_binds: Vec<String>,
}

impl NvidiaState {
    pub fn all_paths(&self) -> Vec<String> {
        let mut paths = self.readonly_binds.clone();
        paths.extend(self.device_binds.clone());
        paths
    }
}

macro_rules! log_step {
    ($name:expr, $step:expr, $msg:expr) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, $msg);
    };
    ($name:expr, $step:expr, $fmt:expr, $($arg:tt)*) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, format!($fmt, $($arg)*));
    };
}

// CDI Parsing Structs for industry-standard discovery (ISO/IEC 20248 compliant)
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdiSpec {
    container_edits: Option<CdiEdits>,
    devices: Option<Vec<CdiDevice>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdiDevice {
    #[allow(dead_code)]
    name: String,
    container_edits: Option<CdiEdits>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdiEdits {
    device_nodes: Option<Vec<CdiDeviceNode>>,
    mounts: Option<Vec<CdiMount>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdiDeviceNode {
    path: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdiMount {
    host_path: String,
}

/// Get the current NVIDIA driver version on the host.
/// Gracefully handles WSL and missing sysfs nodes.
pub async fn get_host_driver_version() -> Result<String> {
    let path = "/sys/module/nvidia/version";
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Ok(s.trim().to_string()),
        Err(_) => {
            log::debug!(
                "Could not read host driver version from {}, assuming unknown/WSL",
                path
            );
            Ok("unknown_or_wsl".to_string())
        }
    }
}

/// Perform a comprehensive scan of the host using the official NVIDIA CDI standard.
pub async fn get_nvidia_state() -> Result<NvidiaState> {
    let mut state = NvidiaState {
        driver_version: get_host_driver_version().await.unwrap_or_default(),
        ..Default::default()
    };

    // 1. CDI Discovery: Call nvidia-ctk to get the official mapping JSON via a temp file
    let tmp_path = format!("/tmp/lasper-cdi-{}.json", std::process::id());
    let out = new_command("nvidia-ctk")
        .args(["cdi", "generate", "--format=json", &format!("--output={}", tmp_path)])
        .output()
        .await
        .map_err(|e| {
            NspawnError::Runtime(format!("Failed to execute 'nvidia-ctk': {}. Please ensure nvidia-container-toolkit is installed.", e))
        })?;

    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(NspawnError::cmd_failed(
            "NVIDIA CDI Discovery",
            format!(
                "nvidia-ctk cdi generate --format=json --output={}",
                tmp_path
            ),
            &out,
        ));
    }

    // Check if the file was actually created and has content
    let path = std::path::Path::new(&tmp_path);
    if !path.exists() {
        return Err(NspawnError::Runtime(format!(
            "nvidia-ctk reported success but no CDI file was created at {}",
            tmp_path
        )));
    }

    let content = tokio::fs::read(&tmp_path)
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from(&tmp_path), e))?;

    if content.is_empty() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        log::warn!("nvidia-ctk generated an empty CDI file. Assuming no NVIDIA devices are present or driver is inactive.");
        return Ok(state);
    }

    let spec: CdiSpec = match serde_json::from_slice(&content) {
        Ok(s) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            s
        }
        Err(e) => {
            log::error!(
                "CDI Raw Output (saved at {}): {}",
                tmp_path,
                String::from_utf8_lossy(&content)
            );
            return Err(NspawnError::Runtime(format!(
                "Failed to parse CDI JSON (check {}): {}",
                tmp_path, e
            )));
        }
    };

    // Collect edits from top-level or from devices
    let mut all_edits = Vec::new();
    if let Some(edits) = spec.container_edits {
        all_edits.push(edits);
    }
    if let Some(devices) = spec.devices {
        for dev in devices {
            if let Some(edits) = dev.container_edits {
                all_edits.push(edits);
            }
        }
    }

    for edits in all_edits {
        if let Some(nodes) = edits.device_nodes {
            for node in nodes {
                state.device_binds.push(node.path);
            }
        }
        if let Some(mounts) = edits.mounts {
            for mount in mounts {
                state.readonly_binds.push(mount.host_path);
            }
        }
    }

    // 2. Symlink Magic: Resolve aliases for .so files to ensure compatibility
    let mut resolved_libs = Vec::new();
    for path in &state.readonly_binds {
        if path.contains(".so") {
            if let Ok(aliases) = resolve_so_aliases(path).await {
                resolved_libs.extend(aliases);
            }
        }
    }
    state.readonly_binds.extend(resolved_libs);

    // 3. Cleanup and dedup
    state.device_binds = dedup(state.device_binds);
    state.readonly_binds = dedup(state.readonly_binds);

    Ok(state)
}

pub async fn cleanup_container_garbage(name: &str, death_list: &[String]) -> Result<()> {
    log_step!(
        name,
        "Cleanup",
        "Inspecting and removing 0-byte driver files from host..."
    );

    // 1. Mount rootfs
    let backend = crate::nspawn::utils::storage::get_storage_backend_for(name);
    let rootfs = backend.mount(name).await?;

    // 2. Precise cleanup: Iterate and remove 0-byte files
    for path in death_list {
        let target = rootfs.join(path.trim_start_matches('/'));
        if target.exists() {
            if let Ok(meta) = tokio::fs::metadata(&target).await {
                if meta.len() == 0 {
                    log_step!(name, "Cleanup", "Deleting 0-byte junk: {}", path);
                    let _ = tokio::fs::remove_file(&target).await;
                }
            }
        }
    }

    // 3. Fallback scan for broad cleanup (precision host-side find)
    let scan_dirs = ["usr/lib", "etc", "usr/share"];
    for dir in scan_dirs {
        let dir_path = rootfs.join(dir);
        if dir_path.exists() {
            let _ = cleanup_recursive_0byte(&dir_path).await;
        }
    }

    // 4. Unmount
    let _ = backend.unmount(name).await;

    Ok(())
}

async fn cleanup_recursive_0byte(path: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(path)
        .await
        .map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        if p.is_dir() {
            let _ = Box::pin(cleanup_recursive_0byte(&p)).await;
        } else {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if (name.contains("nvidia") || name.contains("cuda"))
                && entry.metadata().await.map(|m| m.len()).unwrap_or(1) == 0
            {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }
    }
    Ok(())
}

fn get_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LASPER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("lasper").join("states");
    }
    if let Some(home) = dirs::home_dir() {
        home.join(".local")
            .join("state")
            .join("lasper")
            .join("states")
    } else {
        PathBuf::from("/var/lib/lasper/states")
    }
}

pub async fn get_external_state(name: &str) -> Result<Option<NvidiaState>> {
    let dir = get_state_dir();
    let path = dir.join(format!("{}.json", name));
    if !path.exists() {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| NspawnError::Io(path.clone(), e))?;
    let state: NvidiaState = serde_json::from_str(&content)?;
    Ok(Some(state))
}

pub async fn get_internal_state(name: &str) -> Result<Option<NvidiaState>> {
    let backend = crate::nspawn::utils::storage::get_storage_backend_for(name);
    let rootfs = backend.mount(name).await?;
    let path = rootfs.join("etc/.lasper-nvidia.json");

    let res = if path.exists() {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    let _ = backend.unmount(name).await;
    Ok(res)
}

pub async fn save_external_state(name: &str, state: &NvidiaState) -> Result<()> {
    let dir = get_state_dir();
    let _ = tokio::fs::create_dir_all(&dir).await;
    let path = dir.join(format!("{}.json", name));
    let content = serde_json::to_string_pretty(state)?;
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| NspawnError::Io(path, e))?;
    Ok(())
}

pub async fn save_internal_state(name: &str, state: &NvidiaState) -> Result<()> {
    let content = serde_json::to_string_pretty(state)?;

    let backend = crate::nspawn::utils::storage::get_storage_backend_for(name);
    let rootfs = backend.mount(name).await?;
    let path = rootfs.join("etc/.lasper-nvidia.json");

    if let Err(e) = tokio::fs::write(&path, content).await {
        let _ = backend.unmount(name).await;
        return Err(NspawnError::Io(path, e));
    }

    let _ = backend.unmount(name).await;
    Ok(())
}

pub fn calculate_death_list(old: &NvidiaState, new: &NvidiaState) -> Vec<String> {
    let old_paths = old.all_paths();
    let new_paths = new.all_paths();
    old_paths
        .into_iter()
        .filter(|p| !new_paths.contains(p))
        .collect()
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    dbus: &crate::nspawn::core::provider::dbus::DbusProvider,
) -> Result<()> {
    // 1. Semantic Marker Check
    let config = match crate::nspawn::config::nspawn_file::NspawnConfig::load(name) {
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

    tokio::fs::write(&path, content)
        .await
        .map_err(|e| NspawnError::Io(path, e))?;

    // Cleanup old transient one if present
    let transient_path = format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d/10-lasper-nvidia.conf",
        name
    );
    let _ = tokio::fs::remove_file(transient_path).await;

    Ok(())
}

/// For a given .so path (which might be a versioned file), find it and all its aliases in the same dir.
/// E.g. if we have libcuda.so.595.58.03, we also want libcuda.so.1 and libcuda.so if they point to it.
async fn resolve_so_aliases(path: &str) -> Result<Vec<String>> {
    let p = Path::new(path);
    let dir = p
        .parent()
        .ok_or_else(|| NspawnError::Runtime("Invalid lib path".into()))?;
    let file_name = p
        .file_name()
        .ok_or_else(|| NspawnError::Runtime("Invalid lib path".into()))?
        .to_string_lossy();

    // Extract base name, e.g. "libcuda.so" from "libcuda.so.595.58.03"
    let base_name = if let Some(pos) = file_name.find(".so") {
        &file_name[..pos + 3]
    } else {
        &file_name
    };

    let mut aliases = Vec::new();
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| NspawnError::Io(dir.to_path_buf(), e))?;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| NspawnError::Io(dir.to_path_buf(), e))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(base_name) {
            aliases.push(entry.path().to_string_lossy().into_owned());
        }
    }
    Ok(aliases)
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_real_world_cdi_json() {
        let json = r#"{"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"0","containerEdits":{"deviceNodes":[{"path":"/dev/nvidia0"}]}}],"containerEdits":{"env":["NVIDIA_VISIBLE_DEVICES=void"],"deviceNodes":[{"path":"/dev/nvidiactl"}]}}"#;
        let spec: CdiSpec = serde_json::from_str(json).unwrap();

        let mut nodes = Vec::new();
        if let Some(edits) = spec.container_edits {
            for node in edits.device_nodes.unwrap() {
                nodes.push(node.path);
            }
        }
        for dev in spec.devices.unwrap() {
            for node in dev.container_edits.unwrap().device_nodes.unwrap() {
                nodes.push(node.path);
            }
        }

        assert!(nodes.contains(&"/dev/nvidiactl".to_string()));
        assert!(nodes.contains(&"/dev/nvidia0".to_string()));
    }
}
