use super::cdi::{CdiDeviceNode, CdiMount, CdiSpec};
use super::classify::{self, ClassifiedEntry};
use super::profile::{NvidiaPassthroughMode, NvidiaPassthroughProfile};
use super::resolve::{get_ldconfig_cache, resolve_so_aliases};
use super::state::{NvidiaState, PassthroughBind};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{new_command, CommandLogged};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Check whether `nvidia-ctk` is available on PATH.
pub fn nvidia_ctk_available() -> bool {
    which::which("nvidia-ctk").is_ok()
}

/// Get the current NVIDIA driver version on the host.
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
            let clean_id = id.split('=').next_back().unwrap_or(id);
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

/// Convert a parsed CDI spec into mirror-mode `PassthroughBind` entries.
/// Remapping is NOT applied here — call `remap_binds` afterwards if needed.
fn cdi_to_raw_binds(spec: &CdiSpec) -> Vec<PassthroughBind> {
    let mut binds: Vec<PassthroughBind> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push = |binds: &mut Vec<PassthroughBind>, b: PassthroughBind| {
        if seen.insert(b.container_path.clone()) {
            binds.push(b);
        }
    };

    let (all_mounts, all_hooks, all_device_nodes) = collect_cdi_edits(spec);

    // Device nodes → Bind (read-write)
    for node in &all_device_nodes {
        let path = &node.path;
        let host = node.host_path.as_deref().unwrap_or(path);
        push(
            &mut binds,
            PassthroughBind {
                host_path: host.to_string(),
                container_path: path.to_string(),
                readonly: false,
            },
        );
    }

    // Mounts → BindReadOnly
    let mut mount_map: HashMap<String, String> = HashMap::new();
    for m in &all_mounts {
        mount_map.insert(m.container_path.clone(), m.host_path.clone());
    }

    let (classified, unclassified) = classify::classify_mounts(all_mounts);
    for ce in &classified {
        push(
            &mut binds,
            PassthroughBind {
                host_path: ce.host_path.clone(),
                container_path: ce.default_container_path.clone(),
                readonly: true,
            },
        );
    }
    for m in unclassified {
        push(
            &mut binds,
            PassthroughBind {
                host_path: m.source,
                container_path: m.target,
                readonly: true,
            },
        );
    }

    // Symlink hooks -> synthetic BindReadOnly
    let symlinks = classify::parse_symlink_hooks(&all_hooks);
    for sym in &symlinks {
        let host_target = resolve_symlink_host_path(&sym.target, &sym.link_path, &mount_map);
        if let Some(host_path) = host_target {
            push(
                &mut binds,
                PassthroughBind {
                    host_path,
                    container_path: sym.link_path.clone(),
                    readonly: true,
                },
            );
        } else {
            log::warn!(
                "CDI symlink target '{}' (for link '{}') not found in CDI mounts — skipping",
                sym.target,
                sym.link_path
            );
        }
    }

    binds
}

/// Collect all CDI edits from the top-level container_edits and per-device container_edits.
fn collect_cdi_edits(
    spec: &CdiSpec,
) -> (Vec<CdiMount>, Vec<super::cdi::CdiHook>, Vec<CdiDeviceNode>) {
    let mut mounts = Vec::new();
    let mut hooks = Vec::new();
    let mut device_nodes = Vec::new();

    if let Some(ref edits) = spec.container_edits {
        if let Some(ref m) = edits.mounts {
            mounts.extend_from_slice(m);
        }
        if let Some(ref h) = edits.hooks {
            hooks.extend_from_slice(h);
        }
        if let Some(ref n) = edits.device_nodes {
            device_nodes.extend_from_slice(n);
        }
    }

    if let Some(ref devices) = spec.devices {
        for dev in devices {
            if let Some(ref edits) = dev.container_edits {
                if let Some(ref m) = edits.mounts {
                    mounts.extend_from_slice(m);
                }
                if let Some(ref h) = edits.hooks {
                    hooks.extend_from_slice(h);
                }
                if let Some(ref n) = edits.device_nodes {
                    device_nodes.extend_from_slice(n);
                }
            }
        }
    }

    (mounts, hooks, device_nodes)
}

/// Resolve a symlink target to a host path.
///
/// If target is absolute, look it up directly in the mount map (container_path -> host_path).
/// If target is relative, resolve it against link_path's parent directory first.
fn resolve_symlink_host_path(
    target: &str,
    link_path: &str,
    mount_map: &HashMap<String, String>,
) -> Option<String> {
    if target.starts_with('/') {
        return mount_map.get(target).cloned();
    }

    // Relative target: resolve against link_path's parent
    let parent = Path::new(link_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let resolved = if parent.is_empty() || parent == "/" {
        format!("/{}", target)
    } else {
        format!("{}/{}", parent, target)
    };
    mount_map.get(&resolved).cloned()
}

/// Remap container_path in each bind based on profile's category destinations.
fn remap_binds(binds: &mut [PassthroughBind], profile: &NvidiaPassthroughProfile) {
    for bind in binds.iter_mut() {
        // Only remap read-only binds (not device nodes)
        if !bind.readonly {
            continue;
        }

        let category = classify::classify_path(&bind.container_path);

        if let Some(cat) = category {
            if let Some(dest_dir) = profile.category_destinations.get(&cat) {
                let root = cat.default_container_root();
                let dest = dest_dir.trim_end_matches('/');

                if !root.is_empty() && bind.container_path.starts_with(root) {
                    let relative = &bind.container_path[root.len()..];
                    bind.container_path = format!("{}{}", dest, relative);
                } else if !root.is_empty() {
                    // Path doesn't start with root — just use filename
                    let filename = bind
                        .container_path
                        .split('/')
                        .next_back()
                        .unwrap_or_default();
                    bind.container_path = format!("{}/{}", dest, filename);
                }
                // root.is_empty() -> Config, keep original container path
            }
        }
    }
}

/// Build a classified_entries list from binds for backward compat with UI consumers.
fn extract_classified_entries(binds: &[PassthroughBind]) -> Vec<ClassifiedEntry> {
    binds
        .iter()
        .filter_map(|b| {
            classify::classify_path(&b.container_path).map(|category| ClassifiedEntry {
                host_path: b.host_path.clone(),
                default_container_path: b.container_path.clone(),
                category,
            })
        })
        .collect()
}

/// Perform a comprehensive scan of the host using the official NVIDIA CDI standard.
pub async fn get_nvidia_state(profile: Option<&NvidiaPassthroughProfile>) -> Result<NvidiaState> {
    let driver_version = get_host_driver_version().await.unwrap_or_default();
    let gpu_device = profile.map(|p| p.gpu_device.as_str()).unwrap_or("all");

    // 1. CDI Discovery: Call nvidia-ctk to get the official mapping JSON
    let tmp_dir = tempfile::tempdir().map_err(|e| {
        NspawnError::Runtime(format!(
            "Failed to create temporary directory for CDI discovery: {}",
            e
        ))
    })?;
    let tmp_path = tmp_dir.path().join("nvidia-cdi.json");
    let tmp_path_str = tmp_path.to_string_lossy();

    let mut cmd = new_command("nvidia-ctk");
    cmd.args([
        "cdi",
        "generate",
        "--format=json",
        "--output",
        &tmp_path_str,
    ]);
    if gpu_device != "all" {
        cmd.args(["--device-id", gpu_device]);
    }

    let out = cmd.logged_output("nvidia-ctk").await.map_err(|e| {
        NspawnError::Runtime(format!(
            "Failed to execute 'nvidia-ctk': {}. Please ensure nvidia-container-toolkit is installed.",
            e
        ))
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
        log::warn!(
            "nvidia-ctk generated an empty CDI file. Assuming no NVIDIA devices are present."
        );
        return Ok(NvidiaState {
            driver_version,
            ..Default::default()
        });
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

    let (_, all_hooks, _) = collect_cdi_edits(&spec);

    // 2. Core transform: CDI -> mirror-mode PassthroughBind (no remapping yet)
    let mut binds = cdi_to_raw_binds(&spec);

    // 3. ldconfig alias resolution - add genuinely new .so files not already
    // covered by CDI mounts or symlink hooks. Runs BEFORE remapping so
    // ldconfig-found libraries (e.g. lib32) get remapped uniformly.
    let mut seen_container_paths: HashSet<String> =
        binds.iter().map(|b| b.container_path.clone()).collect();
    let mut extra_binds: Vec<PassthroughBind> = Vec::new();

    if let Some(ldconfig_cache) = get_ldconfig_cache().await {
        for bind in &binds {
            if !bind.host_path.contains(".so") {
                continue;
            }
            let Ok(aliases) = resolve_so_aliases(&bind.host_path, Some(&ldconfig_cache)).await
            else {
                continue;
            };
            for alias in aliases {
                if !seen_container_paths.insert(alias.clone()) {
                    continue;
                }
                extra_binds.push(PassthroughBind {
                    host_path: alias.clone(),
                    container_path: alias,
                    readonly: true,
                });
            }
        }
    }
    binds.extend(extra_binds);

    // 4. Apply category remapping uniformly to all binds (CDI + ldconfig)
    if let Some(prof) = profile {
        if prof.mode == NvidiaPassthroughMode::Categorized {
            remap_binds(&mut binds, prof);
        }
    }

    let mut state = NvidiaState {
        driver_version,
        binds,
        profile: profile.cloned(),
        ..Default::default()
    };

    // Parse hooks metadata (not used for binds, but stored for ldconfig/env injection)
    state.ldcache_folders = classify::parse_ldcache_folders(&all_hooks);
    // env_vars are parsed below - they're in the spec.container_edits.env
    // We can't call classify::parse_env_vars here because we don't have the env from spec.

    // Populate legacy fields for backward compat, then re-derive classified_entries
    // (populate_legacy clears them, so classified_entries must be computed AFTER)
    state.populate_legacy();
    state.classified_entries = extract_classified_entries(&state.binds);

    // 4. Env vars
    if let Some(ref edits) = spec.container_edits {
        if let Some(ref env) = edits.env {
            state.env_vars = classify::parse_env_vars(env);
        }
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::super::cdi::{CdiDeviceNode, CdiHook};
    use super::super::classify::NvidiaFileCategory;
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
        assert!(dedup(Vec::new()).is_empty());
    }

    #[test]
    fn test_dedup_already_unique() {
        let input = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let result = dedup(input.clone());
        assert_eq!(result, input);
    }

    #[test]
    fn test_resolve_absolute_symlink() {
        let mut map = HashMap::new();
        map.insert(
            "/usr/lib/wsl/drivers/nvidia-smi".into(),
            "/host/wsl/nvidia-smi".into(),
        );
        let result = resolve_symlink_host_path(
            "/usr/lib/wsl/drivers/nvidia-smi",
            "/usr/bin/nvidia-smi",
            &map,
        );
        assert_eq!(result, Some("/host/wsl/nvidia-smi".into()));
    }

    #[test]
    fn test_resolve_relative_symlink() {
        let mut map = HashMap::new();
        map.insert(
            "/usr/lib/libcuda.so.595.58.03".into(),
            "/host/drivers/libcuda.so.595.58.03".into(),
        );
        let result =
            resolve_symlink_host_path("libcuda.so.595.58.03", "/usr/lib/libcuda.so.1", &map);
        assert_eq!(result, Some("/host/drivers/libcuda.so.595.58.03".into()));
    }

    #[test]
    fn test_resolve_symlink_not_found() {
        let map = HashMap::new();
        let result = resolve_symlink_host_path("/nonexistent/path", "/usr/bin/foo", &map);
        assert_eq!(result, None);
    }

    #[test]
    fn test_cdi_raw_binds_device_nodes() {
        let spec = CdiSpec {
            container_edits: Some(super::super::cdi::CdiEdits {
                device_nodes: Some(vec![CdiDeviceNode {
                    path: "/dev/nvidia0".into(),
                    host_path: None,
                    major: None,
                    minor: None,
                    permissions: None,
                    gid: None,
                }]),
                mounts: None,
                hooks: None,
                env: None,
            }),
            devices: None,
        };

        let binds = cdi_to_raw_binds(&spec);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].container_path, "/dev/nvidia0");
        assert_eq!(binds[0].host_path, "/dev/nvidia0");
        assert!(!binds[0].readonly);
    }

    #[test]
    fn test_cdi_raw_binds_symlink_synthesis() {
        // Simulate a CDI spec with a mount and a matching symlink hook
        let mount = CdiMount {
            host_path: "/host/libcuda.so.595.58.03".into(),
            container_path: "/usr/lib/libcuda.so.595.58.03".into(),
            options: None,
        };
        let hook = CdiHook {
            hook_name: "createContainer".into(),
            path: "/usr/bin/nvidia-cdi-hook".into(),
            args: Some(vec![
                "nvidia-cdi-hook".into(),
                "create-symlinks".into(),
                "--link".into(),
                "libcuda.so.595.58.03::/usr/lib/libcuda.so.1".into(),
            ]),
        };

        let spec = CdiSpec {
            container_edits: Some(super::super::cdi::CdiEdits {
                device_nodes: None,
                mounts: Some(vec![mount]),
                hooks: Some(vec![hook]),
                env: None,
            }),
            devices: None,
        };

        let binds = cdi_to_raw_binds(&spec);
        // Should have: 1 mount bind + 1 symlink bind
        assert!(
            binds.len() >= 2,
            "expected at least 2 binds, got {}",
            binds.len()
        );
        let sym_bind = binds
            .iter()
            .find(|b| b.container_path == "/usr/lib/libcuda.so.1")
            .expect("symlink bind should exist");
        assert_eq!(sym_bind.host_path, "/host/libcuda.so.595.58.03");
        assert!(sym_bind.readonly);
    }

    #[test]
    fn test_classify_path_used_by_discovery() {
        assert_eq!(
            classify::classify_path("/usr/lib/libcuda.so"),
            Some(NvidiaFileCategory::Lib64)
        );
        assert_eq!(
            classify::classify_path("/usr/bin/nvidia-smi"),
            Some(NvidiaFileCategory::Bin)
        );
        assert_eq!(
            classify::classify_path("/lib/firmware/nvidia/gsp.bin"),
            Some(NvidiaFileCategory::Firmware)
        );
        assert_eq!(
            classify::classify_path("/etc/vulkan/icd.d/nvidia_icd.json"),
            Some(NvidiaFileCategory::Config)
        );
        assert_eq!(
            classify::classify_path("/usr/share/nvidia/nvoptix.bin"),
            None
        );
    }
}
