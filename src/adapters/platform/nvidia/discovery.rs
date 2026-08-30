use super::cdi::{CdiDeviceNode, CdiMount, CdiSpec};
use super::classify::{self, ClassifiedEntry};
use super::resolve::{get_ldconfig_cache, resolve_so_aliases};
use super::state::{NvidiaState, PassthroughBind};
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::new_command;
use crate::domain::nvidia::{NvidiaPassthroughMode, NvidiaPassthroughProfile};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

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

/// Refresh NVIDIA hardware from one current CDI snapshot.
pub async fn discover_hardware() -> Result<(Vec<String>, NvidiaState)> {
    let driver_version = get_host_driver_version().await.unwrap_or_default();
    let spec = generate_cdi_spec("all").await?;

    let devices = devices_from_spec(&spec);
    let state = build_nvidia_state(&spec, driver_version, None).await?;
    validate_authoritative_state(&state, "all")?;
    Ok((devices, state))
}

pub(crate) fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn devices_from_spec(spec: &CdiSpec) -> Vec<String> {
    let devices = spec
        .devices
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|device| device.name.clone())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let mut devices = dedup(devices);
    if let Some(index) = devices.iter().position(|device| device == "all") {
        let all = devices.remove(index);
        devices.insert(0, all);
    }
    devices
}

/// Container paths that conflict with distribution-provided libraries
/// (e.g. Mesa).  These are NOT bind-mounted — the container should use its
/// own version.
const NVIDIA_CDI_SKIP_CONTAINER_PATHS: &[&str] = &["/usr/lib/libGLX_indirect.so.0"];

fn is_conflict_container_path(path: &str) -> bool {
    NVIDIA_CDI_SKIP_CONTAINER_PATHS.contains(&path)
}

/// Convert a parsed CDI spec into mirror-mode `PassthroughBind` entries.
/// Remapping is NOT applied here — call `remap_binds` afterwards if needed.
fn cdi_to_raw_binds(spec: &CdiSpec) -> Vec<PassthroughBind> {
    let mut binds: Vec<PassthroughBind> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push = |binds: &mut Vec<PassthroughBind>, b: PassthroughBind| {
        if is_conflict_container_path(&b.container_path) {
            log::debug!(
                "Skipping CDI entry with conflicting container path: {}",
                b.container_path
            );
            return;
        }
        if seen.insert(b.container_path.clone()) {
            binds.push(b);
        }
    };

    let (all_mounts, all_hooks, all_device_nodes) = collect_cdi_edits(spec);
    let mut source_map: HashMap<String, String> = HashMap::new();
    let mut writable_sources: HashSet<String> = HashSet::new();

    // Device nodes → Bind (read-write)
    for node in &all_device_nodes {
        let path = &node.path;
        let host = node.host_path.as_deref().unwrap_or(path);
        source_map.insert(path.clone(), host.to_string());
        writable_sources.insert(path.clone());
        push(
            &mut binds,
            PassthroughBind {
                host_path: host.to_string(),
                container_path: path.to_string(),
                readonly: false,
            },
        );
    }

    // Mounts preserve the OCI ro/rw option. Mounts are writable unless the
    // option sequence resolves to ro.
    for m in &all_mounts {
        source_map.insert(m.container_path.clone(), m.host_path.clone());
        if !m.readonly() {
            writable_sources.insert(m.container_path.clone());
        }
    }

    let (classified, unclassified) = classify::classify_mounts(all_mounts);
    for ce in &classified {
        push(
            &mut binds,
            PassthroughBind {
                host_path: ce.host_path.clone(),
                container_path: ce.default_container_path.clone(),
                readonly: ce.readonly,
            },
        );
    }
    for m in unclassified {
        push(
            &mut binds,
            PassthroughBind {
                host_path: m.source,
                container_path: m.target,
                readonly: m.readonly,
            },
        );
    }

    // Symlink hooks -> synthetic binds backed by the terminal mount/device source.
    let symlinks = classify::parse_symlink_hooks(&all_hooks);
    let symlink_map: HashMap<String, String> = symlinks
        .iter()
        .map(|symlink| (symlink.link_path.clone(), symlink.target.clone()))
        .collect();
    let mut unresolved_symlinks = 0usize;
    for sym in &symlinks {
        let host_target =
            resolve_symlink_source(&sym.target, &sym.link_path, &source_map, &symlink_map);
        if let Some((host_path, source_path)) = host_target {
            push(
                &mut binds,
                PassthroughBind {
                    host_path,
                    container_path: sym.link_path.clone(),
                    readonly: !writable_sources.contains(&source_path),
                },
            );
        } else {
            unresolved_symlinks += 1;
            log::debug!(
                "CDI symlink target '{}' (for link '{}') not found in CDI mounts — skipping",
                sym.target,
                sym.link_path
            );
        }
    }
    if unresolved_symlinks > 0 {
        log::warn!(
            "Skipped {} NVIDIA CDI symlink entries whose targets were not present in the selected CDI edits",
            unresolved_symlinks
        );
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

/// Resolve one symlink target directly against a container-path-to-host-path map.
#[cfg(test)]
fn resolve_symlink_host_path(
    target: &str,
    link_path: &str,
    mount_map: &HashMap<String, String>,
) -> Option<String> {
    resolve_symlink_source(target, link_path, mount_map, &HashMap::new())
        .map(|(host_path, _)| host_path)
}

fn resolve_symlink_source(
    target: &str,
    link_path: &str,
    source_map: &HashMap<String, String>,
    symlink_map: &HashMap<String, String>,
) -> Option<(String, String)> {
    let mut current = resolve_container_symlink_target(target, link_path)?;
    let mut visited = HashSet::new();

    loop {
        if let Some(host_path) = source_map.get(&current) {
            return Some((host_path.clone(), current));
        }
        if !visited.insert(current.clone()) {
            return None;
        }
        let next_target = symlink_map.get(&current)?;
        current = resolve_container_symlink_target(next_target, &current)?;
    }
}

fn resolve_container_symlink_target(target: &str, link_path: &str) -> Option<String> {
    let path = if target.starts_with('/') {
        PathBuf::from(target)
    } else {
        Path::new(link_path).parent()?.join(target)
    };
    normalize_absolute_container_path(&path)
}

fn normalize_absolute_container_path(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part),
            Component::Prefix(_) => return None,
        }
    }

    let mut normalized = PathBuf::from("/");
    for part in parts {
        normalized.push(part);
    }
    Some(normalized.to_string_lossy().into_owned())
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

/// Apply manual reclassifications from the profile.
/// Overrides container_path and readonly flag for matched binds.
/// Only touches binds whose container_path is currently unclassified —
/// skips binds that already have a classified path (e.g. from symlink hooks),
/// since those already carry the correct FHS mapping.
/// Runs before category-based remapping so user-assigned categories
/// can then participate in `remap_binds`.
fn apply_manual_classifications(binds: &mut [PassthroughBind], profile: &NvidiaPassthroughProfile) {
    for bind in binds.iter_mut() {
        if let Some(mc) = profile
            .manual_classifications
            .iter()
            .find(|mc| mc.host_path == bind.host_path)
        {
            // Skip binds that are already properly classified (e.g. symlink hooks)
            if classify::classify_path(&bind.container_path).is_some() {
                continue;
            }
            if !mc.destination.is_empty() {
                bind.container_path = mc.destination.clone();
            }
            bind.readonly = mc.readonly;
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
                readonly: b.readonly,
            })
        })
        .collect()
}

async fn generate_cdi_spec(gpu_device: &str) -> Result<CdiSpec> {
    let mut cmd = new_command("nvidia-ctk");
    // Keep CDI hooks in the snapshot: the state builder translates their
    // symlink and ld-cache edits into nspawn bind configuration.
    cmd.args(["cdi", "generate", "--format=json"]);
    if gpu_device != "all" {
        cmd.args(["--device-id", gpu_device]);
    }

    let out = cmd.output().await.map_err(|e| {
        NspawnError::Runtime(format!(
            "Failed to execute 'nvidia-ctk': {}. Please ensure nvidia-container-toolkit is installed.",
            e
        ))
    })?;

    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        if out.status.success() {
            log::debug!("[nvidia-ctk stderr] {}", line);
        } else {
            log::warn!("[nvidia-ctk stderr] {}", line);
        }
    }

    if !out.status.success() {
        let device_arg = if gpu_device == "all" {
            String::new()
        } else {
            format!(" --device-id={gpu_device}")
        };
        return Err(NspawnError::cmd_failed(
            "NVIDIA CDI Discovery",
            format!("nvidia-ctk cdi generate --format=json{device_arg}"),
            &out,
        ));
    }

    if out.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(NspawnError::Runtime(
            "nvidia-ctk generated empty CDI JSON; refusing to replace the current NVIDIA state"
                .into(),
        ));
    }

    let spec = parse_generated_cdi_json(&out.stdout)?;
    validate_cdi_selection(&spec, gpu_device)?;
    Ok(spec)
}

fn parse_generated_cdi_json(content: &[u8]) -> Result<CdiSpec> {
    let mut documents = serde_json::Deserializer::from_slice(content).into_iter::<CdiSpec>();
    let first = documents
        .next()
        .transpose()
        .map_err(|error| NspawnError::Runtime(format!("Failed to parse CDI JSON: {error}")))?
        .ok_or_else(|| NspawnError::Runtime("nvidia-ctk generated empty CDI JSON".into()))?;
    for document in documents {
        document
            .map_err(|error| NspawnError::Runtime(format!("Failed to parse CDI JSON: {error}")))?;
    }
    Ok(first)
}

fn validate_cdi_selection(spec: &CdiSpec, gpu_device: &str) -> Result<()> {
    let devices = spec.devices.as_deref().unwrap_or_default();
    if devices.is_empty() {
        return Err(NspawnError::Runtime(
            "NVIDIA CDI JSON contains no devices; refusing to replace the current NVIDIA state"
                .into(),
        ));
    }
    if !devices.iter().any(|device| device.name == gpu_device) {
        return Err(NspawnError::Runtime(format!(
            "NVIDIA CDI JSON does not contain requested device {gpu_device:?}; refusing to replace the current NVIDIA state"
        )));
    }
    Ok(())
}

fn validate_authoritative_state(state: &NvidiaState, gpu_device: &str) -> Result<()> {
    if state.binds.is_empty() {
        return Err(NspawnError::Runtime(format!(
            "NVIDIA CDI device {gpu_device:?} produced no usable bind mounts; refusing to replace the current NVIDIA state"
        )));
    }
    Ok(())
}

/// Perform a comprehensive scan of the host using the official NVIDIA CDI standard.
pub async fn get_nvidia_state(profile: Option<&NvidiaPassthroughProfile>) -> Result<NvidiaState> {
    let driver_version = get_host_driver_version().await.unwrap_or_default();
    let gpu_device = profile.map(|p| p.gpu_device.as_str()).unwrap_or("all");
    let spec = generate_cdi_spec(gpu_device).await?;

    let state = build_nvidia_state(&spec, driver_version, profile).await?;
    validate_authoritative_state(&state, gpu_device)?;
    Ok(state)
}

async fn build_nvidia_state(
    spec: &CdiSpec,
    driver_version: String,
    profile: Option<&NvidiaPassthroughProfile>,
) -> Result<NvidiaState> {
    let (_, all_hooks, _) = collect_cdi_edits(spec);

    // 2. Core transform: CDI -> mirror-mode PassthroughBind (no remapping yet)
    let mut binds = cdi_to_raw_binds(spec);

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

    // 3.5. Apply manual reclassifications before category remapping,
    // so user-assigned files can participate in remap_binds.
    if let Some(prof) = profile {
        if !prof.manual_classifications.is_empty() {
            apply_manual_classifications(&mut binds, prof);
        }
    }

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
    use super::*;
    use crate::domain::nvidia::NvidiaFileCategory;

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
    fn cdi_device_names_are_derived_from_the_generated_snapshot() {
        let spec: CdiSpec = serde_json::from_str(
            r#"{
                "devices": [
                    {"name": "0"},
                    {"name": "all"},
                    {"name": "0"},
                    {"name": "gpu-uuid"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(devices_from_spec(&spec), vec!["all", "0", "gpu-uuid"]);
    }

    #[test]
    fn cdi_device_names_do_not_invent_an_all_selector() {
        let spec: CdiSpec = serde_json::from_str(r#"{"devices":[{"name":"0"}]}"#).unwrap();

        assert_eq!(devices_from_spec(&spec), vec!["0"]);
    }

    #[test]
    fn cdi_selection_must_exist_in_the_generated_snapshot() {
        let spec: CdiSpec = serde_json::from_str(
            r#"{"devices":[{"name":"all"},{"name":"0"},{"name":"GPU-uuid"}]}"#,
        )
        .unwrap();
        assert!(validate_cdi_selection(&spec, "all").is_ok());
        assert!(validate_cdi_selection(&spec, "0").is_ok());
        assert!(validate_cdi_selection(&spec, "GPU-uuid").is_ok());

        let error = validate_cdi_selection(&spec, "missing").unwrap_err();
        assert!(error.to_string().contains("requested device \"missing\""));

        let empty: CdiSpec = serde_json::from_str(r#"{"devices":[]}"#).unwrap();
        assert!(validate_cdi_selection(&empty, "all").is_err());
        let absent: CdiSpec = serde_json::from_str("{}").unwrap();
        assert!(validate_cdi_selection(&absent, "all").is_err());
    }

    #[test]
    fn authoritative_nvidia_state_requires_usable_binds() {
        let empty = NvidiaState::default();
        let error = validate_authoritative_state(&empty, "all").unwrap_err();
        assert!(error.to_string().contains("no usable bind mounts"));

        let state = NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };
        assert!(validate_authoritative_state(&state, "0").is_ok());
    }

    #[test]
    fn generated_cdi_json_parser_accepts_multiple_specs_and_uses_the_full_spec() {
        let content = br#"{"devices":[{"name":"all"}]} {"devices":[{"name":"stale"}]}"#;
        let spec = parse_generated_cdi_json(content).unwrap();
        assert_eq!(spec.devices.unwrap()[0].name, "all");
    }

    #[test]
    fn generated_cdi_json_parser_rejects_invalid_content() {
        assert!(parse_generated_cdi_json(br"not-json").is_err());
        assert!(parse_generated_cdi_json(b" ").is_err());
        assert!(parse_generated_cdi_json(br#"{"devices":[]} trailing"#).is_err());
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
    fn test_resolve_symlink_normalizes_parent_components() {
        let mut map = HashMap::new();
        map.insert(
            "/usr/lib/libnvidia-allocator.so.1".into(),
            "/host/libnvidia-allocator.so.1".into(),
        );

        let result = resolve_symlink_host_path(
            "../libnvidia-allocator.so.1",
            "/usr/lib/gbm/nvidia-drm_gbm.so",
            &map,
        );

        assert_eq!(result, Some("/host/libnvidia-allocator.so.1".into()));
    }

    #[test]
    fn test_resolve_symlink_follows_hook_chain() {
        let sources = HashMap::from([(
            "/usr/lib/libcuda.so.595.58.03".into(),
            "/host/libcuda.so.595.58.03".into(),
        )]);
        let symlinks = HashMap::from([(
            "/usr/lib/libcuda.so.1".into(),
            "libcuda.so.595.58.03".into(),
        )]);

        let result =
            resolve_symlink_source("libcuda.so.1", "/usr/lib/libcuda.so", &sources, &symlinks);

        assert_eq!(
            result,
            Some((
                "/host/libcuda.so.595.58.03".into(),
                "/usr/lib/libcuda.so.595.58.03".into()
            ))
        );
    }

    #[test]
    fn test_resolve_symlink_rejects_cycles_and_root_escape() {
        let sources = HashMap::new();
        let symlinks = HashMap::from([
            ("/usr/lib/a".into(), "b".into()),
            ("/usr/lib/b".into(), "a".into()),
        ]);

        assert!(resolve_symlink_source("b", "/usr/lib/a", &sources, &symlinks).is_none());
        assert!(resolve_container_symlink_target("../../escape", "/usr/link").is_none());
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
    fn cdi_mount_binds_preserve_ro_and_rw_options() {
        let spec = CdiSpec {
            container_edits: Some(super::super::cdi::CdiEdits {
                device_nodes: None,
                mounts: Some(vec![
                    CdiMount {
                        host_path: "/host/libcuda.so".into(),
                        container_path: "/usr/lib/libcuda.so".into(),
                        options: Some(vec!["rbind".into(), "ro".into()]),
                    },
                    CdiMount {
                        host_path: "/host/nvidia-data".into(),
                        container_path: "/var/lib/nvidia-data".into(),
                        options: Some(vec!["rbind".into(), "rw".into()]),
                    },
                ]),
                hooks: None,
                env: None,
            }),
            devices: None,
        };

        let binds = cdi_to_raw_binds(&spec);
        assert!(binds
            .iter()
            .any(|bind| { bind.container_path == "/usr/lib/libcuda.so" && bind.readonly }));
        assert!(binds
            .iter()
            .any(|bind| { bind.container_path == "/var/lib/nvidia-data" && !bind.readonly }));
    }

    #[test]
    fn test_cdi_device_symlink_keeps_writable_binding() {
        let spec = CdiSpec {
            container_edits: Some(super::super::cdi::CdiEdits {
                device_nodes: Some(vec![CdiDeviceNode {
                    path: "/dev/dri/card0".into(),
                    host_path: None,
                    major: None,
                    minor: None,
                    permissions: None,
                    gid: None,
                }]),
                mounts: None,
                hooks: Some(vec![CdiHook {
                    hook_name: "createContainer".into(),
                    path: "/usr/bin/nvidia-cdi-hook".into(),
                    args: Some(vec![
                        "nvidia-cdi-hook".into(),
                        "create-symlinks".into(),
                        "--link".into(),
                        "../card0::/dev/dri/by-path/gpu-card".into(),
                    ]),
                }]),
                env: None,
            }),
            devices: None,
        };

        let binds = cdi_to_raw_binds(&spec);
        let alias = binds
            .iter()
            .find(|bind| bind.container_path == "/dev/dri/by-path/gpu-card")
            .expect("device alias bind should exist");
        assert_eq!(alias.host_path, "/dev/dri/card0");
        assert!(!alias.readonly);
    }

    #[test]
    fn test_cdi_raw_binds_symlink_synthesis() {
        // Simulate a CDI spec with a mount and a matching symlink hook
        let mount = CdiMount {
            host_path: "/host/libcuda.so.595.58.03".into(),
            container_path: "/usr/lib/libcuda.so.595.58.03".into(),
            options: Some(vec!["rbind".into(), "ro".into()]),
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
