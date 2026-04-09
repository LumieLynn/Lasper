use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::new_command;
use super::state::NvidiaState;
use super::cdi::CdiSpec;
use super::resolve::{get_ldconfig_cache, resolve_so_aliases};

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

pub(crate) fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// Perform a comprehensive scan of the host using the official NVIDIA CDI standard.
pub async fn get_nvidia_state() -> Result<NvidiaState> {
    let mut state = NvidiaState {
        driver_version: get_host_driver_version().await.unwrap_or_default(),
        ..Default::default()
    };

    // 1. CDI Discovery: Call nvidia-ctk to get the official mapping JSON via a temp file
    let cache_dir = "/var/cache/lasper";
    let _ = tokio::fs::create_dir_all(cache_dir).await;
    let tmp_path = format!("{}/cdi-{}.json", cache_dir, std::process::id());
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

    // 2. Symlink Magic: Resolve aliases for .so files via ldconfig
    let ldconfig_cache = get_ldconfig_cache().await;
    let mut resolved_libs = Vec::new();
    for path in &state.readonly_binds {
        if path.contains(".so") {
            if let Ok(aliases) = resolve_so_aliases(path, ldconfig_cache.as_deref()).await {
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
