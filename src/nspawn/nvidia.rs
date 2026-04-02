//! NVIDIA GPU and driver detection logic for host passthrough.

use super::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

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

/// Get the current NVIDIA driver version on the host.
pub async fn get_host_driver_version() -> Result<String> {
    let path = "/sys/module/nvidia/version";
    tokio::fs::read_to_string(path)
        .await
        .map(|s| s.trim().to_string())
        .map_err(|e| NspawnError::Io(PathBuf::from(path), e))
}

/// Perform a comprehensive scan of the host using nvidia-container-cli and dynamic directory scans.
pub async fn get_nvidia_state() -> Result<NvidiaState> {
    let mut state = NvidiaState {
        driver_version: get_host_driver_version().await.unwrap_or_default(),
        ..Default::default()
    };

    // 1. Get base mounts from nvidia-container-cli
    let cli_list = run_nvidia_container_cli_list().await?;
    for path in cli_list {
        if path.starts_with("/dev/") {
            state.device_binds.push(path);
        } else {
            state.readonly_binds.push(path);
        }
    }

    // 2. Dynamic scan for Graphics/EGL/Vulkan JSONs
    let standard_dirs = [
        "/usr/share/glvnd/egl_vendor.d",
        "/usr/share/vulkan/icd.d",
        "/usr/share/vulkan/implicit_layer.d",
        "/usr/share/egl/egl_external_platform.d",
        "/etc/vulkan/icd.d",
        "/etc/glvnd/egl_vendor.d",
    ];

    for &dir in &standard_dirs {
        if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_name = entry.file_name().to_string_lossy().to_lowercase();
                if file_name.contains("nvidia") && file_name.ends_with(".json") {
                    state
                        .readonly_binds
                        .push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
    }

    // 3. Resolve symlinks for .so files
    let mut resolved_libs = Vec::new();
    for path in &state.readonly_binds {
        if path.contains(".so") {
            if let Ok(aliases) = resolve_so_aliases(path).await {
                resolved_libs.extend(aliases);
            }
        }
    }
    state.readonly_binds.extend(resolved_libs);

    // 4. Cleanup and dedup
    state.device_binds = dedup(state.device_binds);
    state.readonly_binds = dedup(state.readonly_binds);

    Ok(state)
}

/// Physically remove legacy 0-byte driver files inside the container using systemd-nspawn.
pub async fn cleanup_container_garbage(name: &str, death_list: &[String]) -> Result<()> {
    // Construct a shell script to check for 0-byte files and delete them.
    // Safety: only delete if file exists and has size 0 ( ! -s ).
    // Heuristic: also look for any residual nvidia/cuda 0-byte files.
    let mut script = String::from("for f in");
    for path in death_list {
        script.push_str(&format!(" '{}'", path));
    }
    script.push_str("; do [ -f \"$f\" ] && [ ! -s \"$f\" ] && rm -v \"$f\"; done; ");
    script.push_str("find /usr/lib /etc /usr/share -maxdepth 4 -type f \\( -name '*nvidia*' -o -name '*cuda*' \\) -size 0 -delete -print");

    log::info!("Cleaning up legacy driver files in container {}...", name);

    let out = Command::new("systemd-nspawn")
        .arg("-M")
        .arg(name)
        .arg("-q")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(&script)
        .output()
        .await
        .map_err(|e| {
            NspawnError::Runtime(format!("Failed to execute cleanup in container: {}", e))
        })?;

    if !out.status.success() {
        log::warn!(
            "In-container cleanup reported issues: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            log::debug!("Cleanup output:\n{}", stdout);
        }
    }

    Ok(())
}

async fn run_nvidia_container_cli_list() -> Result<Vec<String>> {
    let out = Command::new("nvidia-container-cli")
        .arg("list")
        .output()
        .await
        .map_err(|_| {
            NspawnError::Runtime(
                "Dependency missing: Please install 'nvidia-container-toolkit' on the host.".into(),
            )
        })?;

    let paths = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(paths)
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
    let out = Command::new("systemd-nspawn")
        .arg("-M")
        .arg(name)
        .arg("-q")
        .arg("--")
        .arg("cat")
        .arg("/etc/.lasper-nvidia.json")
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            let state: NvidiaState = serde_json::from_slice(&o.stdout)?;
            Ok(Some(state))
        }
        _ => Ok(None),
    }
}

pub async fn save_external_state(name: &str, state: &NvidiaState) -> Result<()> {
    let dir = get_state_dir();
    if !dir.exists() {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            log::warn!("Failed to create state directory {}: {}", dir.display(), e);
        }
    }
    let path = dir.join(format!("{}.json", name));
    let content = serde_json::to_string_pretty(state)?;
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| NspawnError::Io(path, e))?;
    Ok(())
}

pub async fn save_internal_state(name: &str, state: &NvidiaState) -> Result<()> {
    let content = serde_json::to_string(state)?;
    // Use base64 to avoid shell escape issues if needed, but JSON is mostly safe.
    // However, to be absolutely safe, we'll use a temp file or similar if we can.
    // For now, let's use a simple sh -c 'cat > ...'
    let out = Command::new("systemd-nspawn")
        .arg("-M")
        .arg(name)
        .arg("-q")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(format!(
            "cat > /etc/.lasper-nvidia.json <<'EOF'\n{}\nEOF",
            content
        ))
        .output()
        .await
        .map_err(|e| NspawnError::Runtime(format!("Failed to write internal state: {}", e)))?;

    if !out.status.success() {
        return Err(NspawnError::Runtime(format!(
            "Failed to save internal state: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
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
    dbus: &crate::nspawn::provider::dbus::DbusProvider,
) -> Result<()> {
    // 1. Semantic Marker Check
    let config = match super::config::NspawnConfig::load(name) {
        Some(c) => c,
        None => return Ok(()),
    };
    if !config.is_gpu_enabled() {
        return Ok(());
    }

    log::info!("GPU Passthrough enabled for {}, checking state...", name);

    // 2. State Diff Engine
    let host_state = get_nvidia_state().await?;
    let external_cache = get_external_state(name).await?.unwrap_or_default();

    // If driver updated on host, we MUST check internal truth
    let mut old_state = external_cache.clone();
    if external_cache.driver_version != host_state.driver_version {
        if let Ok(Some(internal)) = get_internal_state(name).await {
            old_state = internal;
        }
    }

    if old_state.driver_version == host_state.driver_version && !old_state.driver_version.is_empty()
    {
        log::debug!("GPU state match for {}, skipping re-assembly.", name);
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

    // 3. Physical Cleanup
    cleanup_container_garbage(name, &death_list).await?;

    // 4. AST mutation
    super::config::NspawnConfig::update_gpu_passthrough(name, &host_state, &death_list).await?;

    // 5. Persistent Injection & Dual-Track Sync
    save_external_state(name, &host_state).await?;
    save_internal_state(name, &host_state).await?;
    inject_persistent_device_allow(name, &host_state).await?;

    // 6. Zero-overhead Reload
    dbus.reload_daemon().await?;

    Ok(())
}

async fn inject_persistent_device_allow(name: &str, state: &NvidiaState) -> Result<()> {
    let dir = PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        name
    ));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return Err(NspawnError::Io(dir, e));
    }

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
    fn test_dedup() {
        let input = vec!["b".into(), "a".into(), "b".into(), "c".into(), "a".into()];
        let expected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(dedup(input), expected);
    }
}
