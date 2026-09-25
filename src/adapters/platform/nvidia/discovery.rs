use super::cdi::{
    CdiDeviceNode, CdiDocument, CdiMount, CdiSpec, MAX_CDI_DOCUMENT_BYTES, NVIDIA_CDI_KIND,
};
use super::classify::{self, ClassifiedEntry};
use super::resolve::{get_ldconfig_cache, resolve_so_aliases};
use super::state::{NvidiaState, PassthroughBind};
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::new_command;
use crate::domain::nvidia::{NvidiaCdiSource, NvidiaPassthroughMode, NvidiaPassthroughProfile};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

const STANDARD_CDI_DIRECTORIES: &[&str] = &["/etc/cdi", "/var/run/cdi"];
const NVIDIA_CDI_FILENAMES: &[&str] = &["nvidia.yaml", "nvidia.yml", "nvidia.json"];

/// Check whether `nvidia-ctk` is available on PATH.
pub(crate) fn nvidia_ctk_available() -> bool {
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

pub async fn discover_hardware_from(
    source: &NvidiaCdiSource,
) -> Result<(Vec<String>, NvidiaState)> {
    let driver_version = get_host_driver_version().await.unwrap_or_default();
    let full_spec = load_cdi_spec(source).await?;

    let devices = devices_from_spec(&full_spec);
    let spec = select_cdi_device(full_spec, "all")?;
    validate_cdi_selection(&spec, "all")?;
    let state = build_nvidia_state(&spec, driver_version, None).await?;
    validate_authoritative_state(&state, "all")?;
    validate_host_sources(&state).await?;
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

pub(crate) async fn cdi_source_available(source: &NvidiaCdiSource) -> bool {
    if source.validate().is_err() {
        return false;
    }
    match source {
        NvidiaCdiSource::Generate => nvidia_ctk_available(),
        NvidiaCdiSource::Existing { path } => {
            resolve_existing_cdi_path(path.as_deref()).await.is_ok()
        }
    }
}

async fn load_cdi_spec(source: &NvidiaCdiSource) -> Result<CdiSpec> {
    source.validate().map_err(NspawnError::Validation)?;
    let document = match source {
        NvidiaCdiSource::Generate => generate_cdi_document().await?,
        NvidiaCdiSource::Existing { path } => {
            let path = resolve_existing_cdi_path(path.as_deref()).await?;
            log::debug!("Using existing NVIDIA CDI document {}", path.display());
            read_cdi_document(&path).await?
        }
    };
    validate_cdi_document(&document)?;
    Ok(document.into_spec())
}

async fn resolve_existing_cdi_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
            NspawnError::Runtime(format!(
                "Cannot inspect NVIDIA CDI file {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.file_type().is_file() {
            return Err(NspawnError::Validation(format!(
                "NVIDIA CDI source is not a regular file: {}",
                path.display()
            )));
        }
        return Ok(path.to_path_buf());
    }

    let mut matches = Vec::new();
    for directory in STANDARD_CDI_DIRECTORIES {
        for filename in NVIDIA_CDI_FILENAMES {
            let candidate = Path::new(directory).join(filename);
            match tokio::fs::symlink_metadata(&candidate).await {
                Ok(metadata) if metadata.file_type().is_file() => matches.push(candidate),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(NspawnError::Runtime(format!(
                        "Cannot inspect NVIDIA CDI candidate {}: {error}",
                        candidate.display()
                    )))
                }
            }
        }
    }

    match matches.as_slice() {
        [] => Err(NspawnError::Runtime(format!(
            "No existing NVIDIA CDI document was found in {}",
            STANDARD_CDI_DIRECTORIES.join(" or ")
        ))),
        [path] => Ok(path.clone()),
        _ => Err(NspawnError::Validation(format!(
            "Multiple NVIDIA CDI documents were found ({}); set nvidia.cdi-file explicitly",
            matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

async fn generate_cdi_document() -> Result<CdiDocument> {
    let mut cmd = new_command("nvidia-ctk");
    // Keep CDI hooks in the snapshot: the state builder translates their
    // symlink and ld-cache edits into nspawn bind configuration.
    cmd.args(["cdi", "generate", "--format=json"]);

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
        return Err(NspawnError::cmd_failed(
            "NVIDIA CDI Discovery",
            "nvidia-ctk cdi generate --format=json",
            &out,
        ));
    }

    if out.stdout.len() > MAX_CDI_DOCUMENT_BYTES {
        return Err(NspawnError::Validation(format!(
            "nvidia-ctk CDI JSON exceeds {} bytes",
            MAX_CDI_DOCUMENT_BYTES
        )));
    }
    if out.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(NspawnError::Runtime(
            "nvidia-ctk generated empty CDI JSON; refusing to replace the current NVIDIA state"
                .into(),
        ));
    }

    parse_generated_cdi_json(&out.stdout)
}

async fn read_cdi_document(path: &Path) -> Result<CdiDocument> {
    // Open with O_NOFOLLOW so the metadata check and read refer to the same
    // inode. This matters for a root daemon consuming an explicitly supplied
    // path that another process could otherwise retarget between checks.
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .await
        .map_err(|error| {
            NspawnError::Runtime(format!(
                "Cannot open NVIDIA CDI file {}: {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata().await.map_err(|error| {
        NspawnError::Runtime(format!(
            "Cannot inspect NVIDIA CDI file {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI source is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_CDI_DOCUMENT_BYTES as u64 {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI file {} exceeds {} bytes",
            path.display(),
            MAX_CDI_DOCUMENT_BYTES
        )));
    }

    let uid = metadata.uid();
    let mode = metadata.permissions().mode();
    let effective_uid = uzers::get_effective_uid();
    if uid != 0 && uid != effective_uid {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI file {} is owned by uid {}, not root or the effective user",
            path.display(),
            uid
        )));
    }
    if mode & 0o022 != 0 {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI file {} is writable by group or other users",
            path.display()
        )));
    }

    let mut content = Vec::new();
    file.take((MAX_CDI_DOCUMENT_BYTES as u64) + 1)
        .read_to_end(&mut content)
        .await
        .map_err(|error| {
            NspawnError::Runtime(format!(
                "Cannot read NVIDIA CDI file {}: {error}",
                path.display()
            ))
        })?;
    if content.len() > MAX_CDI_DOCUMENT_BYTES {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI file {} exceeds {} bytes",
            path.display(),
            MAX_CDI_DOCUMENT_BYTES
        )));
    }
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        parse_generated_cdi_json(&content).map_err(|error| {
            NspawnError::Runtime(format!(
                "Failed to parse NVIDIA CDI file {}: {error}",
                path.display()
            ))
        })
    } else {
        parse_cdi_yaml(&content).map_err(|error| {
            NspawnError::Runtime(format!(
                "Failed to parse NVIDIA CDI file {}: {error}",
                path.display()
            ))
        })
    }
}

fn parse_generated_cdi_json(content: &[u8]) -> Result<CdiDocument> {
    let documents = if content
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'[')
    {
        serde_json::from_slice::<Vec<CdiDocument>>(content)
            .map_err(|error| NspawnError::Runtime(format!("Failed to parse CDI JSON: {error}")))?
    } else {
        serde_json::Deserializer::from_slice(content)
            .into_iter::<CdiDocument>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| NspawnError::Runtime(format!("Failed to parse CDI JSON: {error}")))?
    };
    if documents.is_empty() {
        return Err(NspawnError::Runtime(
            "nvidia-ctk generated empty CDI JSON".into(),
        ));
    }

    let mut nvidia = documents
        .into_iter()
        .filter(|document| document.kind.as_deref() == Some(NVIDIA_CDI_KIND));
    let document = nvidia.next().ok_or_else(|| {
        NspawnError::Validation("CDI JSON contains no nvidia.com/gpu document".into())
    })?;
    if nvidia.next().is_some() {
        return Err(NspawnError::Validation(
            "CDI JSON contains multiple nvidia.com/gpu documents".into(),
        ));
    }
    Ok(document)
}

fn parse_cdi_yaml(content: &[u8]) -> std::result::Result<CdiDocument, serde_yml::Error> {
    serde_yml::from_slice(content)
}

fn validate_cdi_document(document: &CdiDocument) -> Result<()> {
    if document.kind.as_deref() != Some(NVIDIA_CDI_KIND) {
        return Err(NspawnError::Validation(format!(
            "NVIDIA CDI document has unsupported kind {:?}",
            document.kind
        )));
    }
    if document
        .cdi_version
        .as_deref()
        .is_none_or(|version| version.trim().is_empty())
    {
        return Err(NspawnError::Validation(
            "NVIDIA CDI document has no cdiVersion".into(),
        ));
    }
    Ok(())
}

/// Project one named CDI device while preserving the document-level edits.
/// The generated `all` entry is preferred for the all-device selector; a
/// legacy document without it falls back to the complete device list.
fn select_cdi_device(mut spec: CdiSpec, gpu_device: &str) -> Result<CdiSpec> {
    let devices = spec.devices.take().unwrap_or_default();
    if devices.is_empty() {
        return Err(NspawnError::Runtime(
            "NVIDIA CDI document contains no devices".into(),
        ));
    }

    if gpu_device == "all" {
        if let Some(all) = devices.iter().find(|device| device.name == "all") {
            spec.devices = Some(vec![all.clone()]);
        } else {
            log::warn!(
                "NVIDIA CDI document has no explicit 'all' device; using all device entries"
            );
            spec.devices = Some(devices);
        }
        return Ok(spec);
    }

    let selected = devices
        .into_iter()
        .find(|device| device.name == gpu_device)
        .ok_or_else(|| {
            NspawnError::Runtime(format!(
                "NVIDIA CDI document does not contain requested device {gpu_device:?}"
            ))
        })?;
    spec.devices = Some(vec![selected]);
    Ok(spec)
}

fn validate_cdi_selection(spec: &CdiSpec, gpu_device: &str) -> Result<()> {
    let devices = spec.devices.as_deref().unwrap_or_default();
    if devices.is_empty() {
        return Err(NspawnError::Runtime(
            "NVIDIA CDI JSON contains no devices; refusing to replace the current NVIDIA state"
                .into(),
        ));
    }
    // Older toolkit documents may list only concrete devices and omit the
    // synthetic `all` entry. `select_cdi_device` intentionally projects that
    // complete set for the all-device request, so non-empty is the correct
    // post-selection invariant here.
    if gpu_device == "all" {
        return Ok(());
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

pub async fn get_nvidia_state_from(
    profile: Option<&NvidiaPassthroughProfile>,
    source: &NvidiaCdiSource,
) -> Result<NvidiaState> {
    let driver_version = get_host_driver_version().await.unwrap_or_default();
    let gpu_device = profile.map(|p| p.gpu_device.as_str()).unwrap_or("all");
    let full_spec = load_cdi_spec(source).await?;
    let spec = select_cdi_device(full_spec, gpu_device)?;
    validate_cdi_selection(&spec, gpu_device)?;

    let state = build_nvidia_state(&spec, driver_version, profile).await?;
    validate_authoritative_state(&state, gpu_device)?;
    validate_host_sources(&state).await?;
    Ok(state)
}

async fn validate_host_sources(state: &NvidiaState) -> Result<()> {
    let mut checked = HashSet::new();
    for bind in &state.binds {
        if !checked.insert(bind.host_path.clone()) {
            continue;
        }
        let metadata = match tokio::fs::metadata(&bind.host_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(NspawnError::Runtime(format!(
                    "NVIDIA CDI host source does not exist: {}",
                    bind.host_path
                )))
            }
            Err(error) => {
                return Err(NspawnError::Runtime(format!(
                    "Cannot inspect NVIDIA CDI host source {}: {error}",
                    bind.host_path
                )))
            }
        };
        if metadata.file_type().is_dir() {
            continue;
        }
        if metadata.file_type().is_file()
            || metadata.file_type().is_char_device()
            || metadata.file_type().is_block_device()
        {
            continue;
        }
        return Err(NspawnError::Runtime(format!(
            "NVIDIA CDI host source has unsupported type: {}",
            bind.host_path
        )));
    }
    Ok(())
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
    fn all_device_selection_accepts_documents_without_a_synthetic_all_entry() {
        let spec: CdiSpec =
            serde_json::from_str(r#"{"devices":[{"name":"0"},{"name":"GPU-uuid"}]}"#).unwrap();
        let selected = select_cdi_device(spec, "all").unwrap();
        assert!(validate_cdi_selection(&selected, "all").is_ok());
        assert_eq!(
            selected
                .devices
                .unwrap()
                .into_iter()
                .map(|device| device.name)
                .collect::<Vec<_>>(),
            vec!["0", "GPU-uuid"]
        );
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
    fn generated_cdi_json_parser_selects_the_nvidia_document() {
        let content = br#"{"cdiVersion":"0.5.0","kind":"vendor.example/device","devices":[{"name":"ignored"}]} {"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"all"}]}"#;
        let document = parse_generated_cdi_json(content).unwrap();
        assert_eq!(document.devices.unwrap()[0].name, "all");
    }

    #[test]
    fn generated_cdi_json_parser_accepts_an_array_document() {
        let content =
            br#"[{"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"all"}]}]"#;
        assert_eq!(
            parse_generated_cdi_json(content).unwrap().devices.unwrap()[0].name,
            "all"
        );
    }

    #[test]
    fn generated_cdi_json_parser_rejects_ambiguous_nvidia_documents() {
        let content = br#"{"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"all"}]} {"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"stale"}]}"#;
        assert!(parse_generated_cdi_json(content).is_err());
    }

    #[test]
    fn generated_cdi_json_parser_rejects_invalid_content() {
        assert!(parse_generated_cdi_json(br"not-json").is_err());
        assert!(parse_generated_cdi_json(b" ").is_err());
        assert!(parse_generated_cdi_json(br#"{"devices":[]} trailing"#).is_err());
    }

    #[test]
    fn reference_yaml_uses_the_same_projection_and_hook_translation() {
        let content = include_bytes!("../../../../docs/reference-nvidia-ctk-cdi.yaml");
        let document = parse_cdi_yaml(content).expect("reference CDI YAML should parse");
        validate_cdi_document(&document).unwrap();
        let full = document.into_spec();
        assert_eq!(
            devices_from_spec(&full),
            vec![
                "all".to_string(),
                "0".to_string(),
                "GPU-182ac723-33e8-575e-bc65-ed6ebd99ed52".to_string()
            ]
        );

        let selected = select_cdi_device(full, "0").unwrap();
        let (_, hooks, _) = collect_cdi_edits(&selected);
        let symlinks = classify::parse_symlink_hooks(&hooks);
        let ldcache = classify::parse_ldcache_folders(&hooks);
        assert!(!symlinks.is_empty());
        assert!(!ldcache.is_empty());
        assert!(selected
            .devices
            .as_ref()
            .unwrap()
            .iter()
            .all(|device| device.name == "0"));
    }

    #[test]
    fn selecting_all_prefers_the_explicit_all_entry() {
        let spec: CdiSpec = serde_json::from_str(
            r#"{
                "containerEdits":{"deviceNodes":[{"path":"/dev/common"}]},
                "devices":[
                    {"name":"0","containerEdits":{"deviceNodes":[{"path":"/dev/zero-gpu"}]}},
                    {"name":"all","containerEdits":{"deviceNodes":[{"path":"/dev/all-gpu"}]}}
                ]
            }"#,
        )
        .unwrap();
        let projected = select_cdi_device(spec, "all").unwrap();
        let (_, _, nodes) = collect_cdi_edits(&projected);
        let paths = nodes.into_iter().map(|node| node.path).collect::<Vec<_>>();
        assert!(paths.contains(&"/dev/common".to_string()));
        assert!(paths.contains(&"/dev/all-gpu".to_string()));
        assert!(!paths.contains(&"/dev/zero-gpu".to_string()));
    }

    #[tokio::test]
    async fn existing_cdi_file_is_bounded_and_parsed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nvidia.yaml");
        tokio::fs::write(
            &path,
            "cdiVersion: '0.7.0'\nkind: nvidia.com/gpu\ndevices:\n  - name: all\n    containerEdits:\n      deviceNodes:\n        - path: /dev/nvidia0\n",
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let document = read_cdi_document(&path).await.unwrap();
        validate_cdi_document(&document).unwrap();
        assert_eq!(document.devices.unwrap()[0].name, "all");
    }

    #[tokio::test]
    async fn existing_cdi_file_rejects_content_over_the_bound() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nvidia.yaml");
        tokio::fs::write(&path, vec![b'x'; MAX_CDI_DOCUMENT_BYTES + 1])
            .await
            .unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let error = read_cdi_document(&path).await.unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn host_source_validation_rejects_missing_paths() {
        let state = NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/definitely/missing/lasper-nvidia-source".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };
        let error = validate_host_sources(&state).await.unwrap_err();
        assert!(error.to_string().contains("does not exist"));
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
