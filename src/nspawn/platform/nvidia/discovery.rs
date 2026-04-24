use super::cdi::CdiSpec;
use super::classify;
use super::profile::NvidiaPassthroughProfile;
use super::resolve::{get_ldconfig_cache, resolve_so_aliases};
use super::state::NvidiaState;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{new_command, CommandLogged};

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

/// List available NVIDIA CDI devices.
pub async fn list_devices() -> Result<Vec<String>> {
    let out = new_command("nvidia-ctk")
        .args(["cdi", "list"])
        .logged_output("nvidia-ctk")
        .await
        .map_err(|e| NspawnError::Runtime(format!("Failed to execute 'nvidia-ctk': {}", e)))?;

    if !out.status.success() {
        return Ok(vec!["all".to_string()]);
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut devices = vec!["all".to_string()];
    for line in stdout.lines() {
        if let Some(id) = line.split_whitespace().last() {
            // Strip vendor prefix if present (e.g. nvidia.com/gpu=0 -> 0)
            let clean_id = id.split('=').last().unwrap_or(id);
            devices.push(clean_id.to_string());
        }
    }
    Ok(dedup(devices))
}

pub(crate) fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// Perform a comprehensive scan of the host using the official NVIDIA CDI standard.
pub async fn get_nvidia_state(profile: Option<&NvidiaPassthroughProfile>) -> Result<NvidiaState> {
    let mut state = NvidiaState {
        driver_version: get_host_driver_version().await.unwrap_or_default(),
        ..Default::default()
    };

    let gpu_device = profile.map(|p| p.gpu_device.as_str()).unwrap_or("all");

    // 1. CDI Discovery: Call nvidia-ctk to get the official mapping JSON via a temp dir
    let tmp_dir = tempfile::tempdir().map_err(|e| {
        NspawnError::Runtime(format!(
            "Failed to create temporary directory for CDI discovery: {}",
            e
        ))
    })?;
    let tmp_path = tmp_dir.path().join("nvidia-cdi.json");
    let tmp_path_str = tmp_path.to_string_lossy();

    let mut cmd = new_command("nvidia-ctk");
    cmd.args(["cdi", "generate", "--format=json", "--output", &tmp_path_str]);
    
    // Per-GPU selection
    if gpu_device != "all" {
        cmd.args(["--device-id", gpu_device]);
    }

    let out = cmd.logged_output("nvidia-ctk")
        .await
        .map_err(|e| {
            NspawnError::Runtime(format!("Failed to execute 'nvidia-ctk': {}. Please ensure nvidia-container-toolkit is installed.", e))
        })?;

    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "NVIDIA CDI Discovery",
            format!(
                "nvidia-ctk cdi generate --format=json --output={} --device-id={}",
                tmp_path_str, gpu_device
            ),
            &out,
        ));
    }

    // Check if the file was actually created and has content
    if !tokio::fs::try_exists(&tmp_path).await.unwrap_or(false) {
        return Err(NspawnError::Runtime(format!(
            "nvidia-ctk reported success but no CDI file was created at {}",
            tmp_path_str
        )));
    }

    let content = tokio::fs::read(&tmp_path)
        .await
        .map_err(|e| NspawnError::Io(tmp_path.clone(), e))?;

    if content.is_empty() {
        log::warn!("nvidia-ctk generated an empty CDI file. Assuming no NVIDIA devices are present or driver is inactive.");
        return Ok(state);
    }

    let spec: CdiSpec = match serde_json::from_slice(&content) {
        Ok(s) => s,
        Err(e) => {
            log::error!(
                "CDI Raw Output (saved at {}): {}",
                tmp_path_str,
                String::from_utf8_lossy(&content)
            );
            return Err(NspawnError::Runtime(format!(
                "Failed to parse CDI JSON: {}",
                e
            )));
        }
    };

    // Collect edits from top-level or from devices
    let mut all_mounts = Vec::new();
    let mut all_hooks = Vec::new();
    let mut all_env = Vec::new();

    if let Some(edits) = spec.container_edits {
        if let Some(m) = edits.mounts { all_mounts.extend(m); }
        if let Some(h) = edits.hooks { all_hooks.extend(h); }
        if let Some(e) = edits.env { all_env.extend(e); }
        if let Some(nodes) = edits.device_nodes {
            for node in nodes { state.device_binds.push(node.path); }
        }
    }

    if let Some(devices) = spec.devices {
        for dev in devices {
            if let Some(edits) = dev.container_edits {
                if let Some(m) = edits.mounts { all_mounts.extend(m); }
                if let Some(h) = edits.hooks { all_hooks.extend(h); }
                if let Some(e) = edits.env { all_env.extend(e); }
                if let Some(nodes) = edits.device_nodes {
                    for node in nodes { state.device_binds.push(node.path); }
                }
            }
        }
    }

    // 2. Classification and Parsing
    let (classified, unclassified) = classify::classify_mounts(all_mounts);
    state.classified_entries = classified;
    state.symlinks = classify::parse_symlink_hooks(&all_hooks);
    state.ldcache_folders = classify::parse_ldcache_folders(&all_hooks);
    state.env_vars = classify::parse_env_vars(&all_env);

    // Unclassified entries land in readonly_binds for backward compat surgery
    // (They will also be accessible via state.classified_entries/unclassified later)
    for m in unclassified {
        state.readonly_binds.push(m.source);
    }

    // 3. Symlink Magic: Resolve aliases for .so files via ldconfig (Double Check)
    let ldconfig_cache = get_ldconfig_cache().await;
    let mut resolved_libs = Vec::new();
    
    // We check both raw unclassified and classified libs
    let mut check_paths = state.readonly_binds.clone();
    for entry in &state.classified_entries {
        check_paths.push(entry.host_path.clone());
    }

    for path in check_paths {
        if path.contains(".so") {
            if let Ok(aliases) = resolve_so_aliases(&path, ldconfig_cache.as_deref()).await {
                resolved_libs.extend(aliases);
            }
        }
    }

    // Merge resolved libs if they aren't already covered by classified_entries
    for lib_path in resolved_libs {
        if !state.classified_entries.iter().any(|ce| ce.host_path == lib_path) {
            state.readonly_binds.push(lib_path);
        }
    }

    // 4. Cleanup and dedup
    state.device_binds = dedup(state.device_binds);
    state.readonly_binds = dedup(state.readonly_binds);

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_sorts_and_removes() {
        let input = vec![
            "c".to_string(),
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
        ];
        let result = dedup(input);
        assert_eq!(
            result,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn test_dedup_empty() {
        let input: Vec<String> = vec![];
        let result = dedup(input);
        assert!(result.is_empty());
    }

    #[test]
    fn test_dedup_already_unique() {
        let input = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let result = dedup(input.clone());
        assert_eq!(result, input);
    }
}
