use crate::adapters::config::nspawn_file::{
    nspawn_config_content_from_spec_with_wayland_path, NspawnConfig,
};
use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::filesystem::AsyncLockedWriter;
use crate::adapters::platform::nvidia::NvidiaState;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    ApplyStatus, ContainerConfig, ImageName, MachineName, NspawnConfigSpec, OciNetworkMode,
};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_NSPAWN_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_NVIDIA_BINDS: usize = 16384;
const LASPER_OCI_CONFIG_MARKER: &str =
    "# Managed by Lasper: promoted systemd OCI runtime configuration";

/// Typed read/write access to `.nspawn` configuration files.
#[derive(Clone)]
pub struct NspawnConfigStore {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl NspawnConfigStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub async fn read(&self, name: &str) -> Result<Option<NspawnConfig>> {
        let machine = parse_machine_name(name)?;
        let result = self
            .execute(NspawnConfigOperation::Read(ReadNspawnConfig { machine }))
            .await?;
        Ok(result.content.map(NspawnConfig::from))
    }

    /// Inspect the effective `.nspawn` file using systemd's discovery order.
    pub async fn inspect(&self, name: &str) -> Result<Option<NspawnConfig>> {
        let image = parse_image_name(name)?;
        let result = self
            .execute(NspawnConfigOperation::Inspect(InspectNspawnConfig {
                image,
            }))
            .await?;
        Ok(result.content.map(NspawnConfig::from))
    }

    pub async fn write_generated(
        &self,
        config: &ContainerConfig,
        xdg_runtime: Option<&str>,
        nvidia_state: Option<&NvidiaState>,
    ) -> Result<ApplyStatus> {
        let spec = NspawnConfigSpec::try_from(config)?;
        let result = self
            .execute(NspawnConfigOperation::Write(WriteNspawnConfig {
                spec,
                xdg_runtime: xdg_runtime.map(str::to_string),
                nvidia_state: nvidia_state.cloned(),
            }))
            .await?;
        result
            .apply
            .ok_or_else(|| NspawnError::Runtime("nspawn write returned no apply status".into()))
    }

    pub async fn promote_oci(&self, name: &str, network: OciNetworkMode) -> Result<ApplyStatus> {
        let result = self
            .execute(NspawnConfigOperation::PromoteOci(PromoteOciConfig {
                machine: parse_machine_name(name)?,
                network,
            }))
            .await?;
        result
            .apply
            .ok_or_else(|| NspawnError::Runtime("OCI promotion returned no apply status".into()))
    }

    pub async fn prepare_oci_promotion(&self, name: &str) -> Result<()> {
        self.execute(NspawnConfigOperation::PrepareOciPromotion(
            PrepareOciPromotion {
                machine: parse_machine_name(name)?,
            },
        ))
        .await?;
        Ok(())
    }

    pub async fn update_gpu(
        &self,
        name: &str,
        state: &NvidiaState,
        removed_binds: &[crate::adapters::platform::nvidia::state::PassthroughBind],
    ) -> Result<()> {
        self.execute(NspawnConfigOperation::UpdateGpu(UpdateNspawnGpu {
            machine: parse_machine_name(name)?,
            state: state.clone(),
            removed_binds: removed_binds.to_vec(),
        }))
        .await?;
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        self.execute(NspawnConfigOperation::Remove(RemoveNspawnConfig {
            machine: parse_machine_name(name)?,
        }))
        .await?;
        Ok(())
    }

    /// Reclaim sidecar locks after systemd removes an image's administrator
    /// `.nspawn` file. The configuration itself is never removed here.
    pub async fn cleanup_sidecar_locks(&self, name: &str) -> Result<bool> {
        let result = self
            .execute(NspawnConfigOperation::CleanupSidecarLocks(
                CleanupNspawnSidecarLocks {
                    image: parse_image_name(name)?,
                },
            ))
            .await?;
        Ok(result.sidecars_cleaned.unwrap_or(false))
    }

    async fn execute(&self, operation: NspawnConfigOperation) -> Result<NspawnConfigResult> {
        if let Some(daemon) = &self.daemon {
            daemon
                .nspawn_config(operation)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            execute_nspawn_config_operation(operation, invoking_uid()).await
        }
    }
}

impl std::fmt::Debug for NspawnConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NspawnConfigStore")
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum NspawnConfigOperation {
    Read(ReadNspawnConfig),
    Inspect(InspectNspawnConfig),
    Write(WriteNspawnConfig),
    PrepareOciPromotion(PrepareOciPromotion),
    PromoteOci(PromoteOciConfig),
    UpdateGpu(UpdateNspawnGpu),
    Remove(RemoveNspawnConfig),
    CleanupSidecarLocks(CleanupNspawnSidecarLocks),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadNspawnConfig {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectNspawnConfig {
    image: ImageName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteNspawnConfig {
    spec: NspawnConfigSpec,
    xdg_runtime: Option<String>,
    nvidia_state: Option<NvidiaState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteOciConfig {
    machine: MachineName,
    network: OciNetworkMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareOciPromotion {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateNspawnGpu {
    machine: MachineName,
    state: NvidiaState,
    removed_binds: Vec<crate::adapters::platform::nvidia::state::PassthroughBind>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NspawnConfigResult {
    content: Option<NspawnConfigInspection>,
    #[serde(default)]
    apply: Option<ApplyStatus>,
    #[serde(default)]
    sidecars_cleaned: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NspawnConfigInspection {
    path: PathBuf,
    content: String,
}

impl From<NspawnConfigInspection> for NspawnConfig {
    fn from(inspection: NspawnConfigInspection) -> Self {
        Self {
            path: inspection.path,
            content: inspection.content,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveNspawnConfig {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupNspawnSidecarLocks {
    image: ImageName,
}

pub(crate) async fn execute_nspawn_config_operation(
    operation: NspawnConfigOperation,
    invoking_uid: u32,
) -> Result<NspawnConfigResult> {
    match operation {
        NspawnConfigOperation::Read(request) => {
            let path = nspawn_path(&request.machine);
            Ok(NspawnConfigResult {
                content: read_config_at(&path).await?,
                ..Default::default()
            })
        }
        NspawnConfigOperation::Inspect(request) => Ok(NspawnConfigResult {
            content: read_discovered_config(&request.image).await?,
            ..Default::default()
        }),
        NspawnConfigOperation::Write(request) => {
            let apply = write_generated_at(
                &nspawn_path(&request.spec.machine),
                &request.spec,
                request.xdg_runtime.as_deref(),
                request.nvidia_state.as_ref(),
                invoking_uid,
            )
            .await?;
            Ok(NspawnConfigResult {
                apply: Some(apply),
                ..Default::default()
            })
        }
        NspawnConfigOperation::PromoteOci(request) => {
            let apply = promote_new_oci_config(
                &request.machine,
                request.network,
                &crate::paths::machines_dir(),
                Path::new("/etc/systemd/nspawn"),
                Path::new("/run/systemd/nspawn"),
            )
            .await?;
            Ok(NspawnConfigResult {
                apply: Some(apply),
                ..Default::default()
            })
        }
        NspawnConfigOperation::PrepareOciPromotion(request) => {
            validate_oci_promotion_target(
                &request.machine,
                Path::new("/etc/systemd/nspawn"),
                Path::new("/run/systemd/nspawn"),
            )
            .await?;
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::UpdateGpu(request) => {
            update_gpu_at(
                &nspawn_path(&request.machine),
                request.machine,
                request.state,
                request.removed_binds,
            )
            .await?;
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::Remove(request) => {
            let path = nspawn_path(&request.machine);
            remove_config_at(&path).await?;
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::CleanupSidecarLocks(request) => {
            let paths = discovered_nspawn_paths(&request.image);
            let cleaned = cleanup_sidecar_locks_at(&paths).await?;
            Ok(NspawnConfigResult {
                sidecars_cleaned: Some(cleaned),
                ..Default::default()
            })
        }
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn parse_image_name(name: &str) -> Result<ImageName> {
    ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn nspawn_path(machine: &MachineName) -> PathBuf {
    NspawnConfig::default_path(machine.as_str())
}

fn discovered_nspawn_paths(image: &ImageName) -> [PathBuf; 3] {
    let filename = format!("{}.nspawn", image.as_str());
    [
        PathBuf::from("/etc/systemd/nspawn").join(&filename),
        PathBuf::from("/run/systemd/nspawn").join(&filename),
        crate::paths::machines_dir().join(filename),
    ]
}

async fn read_discovered_config(image: &ImageName) -> Result<Option<NspawnConfigInspection>> {
    read_discovered_config_from(&discovered_nspawn_paths(image)).await
}

async fn read_discovered_config_from(paths: &[PathBuf]) -> Result<Option<NspawnConfigInspection>> {
    for (index, path) in paths.iter().enumerate() {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => {
                validate_content_size(&content)?;
                return Ok(Some(NspawnConfigInspection {
                    path: path.clone(),
                    content,
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // `/var/lib/machines` is often deliberately inaccessible to the
            // unprivileged UI process. It is an optional inspection source,
            // so an inaccessible adjacent file does not hide trusted config.
            Err(error)
                if index == paths.len().saturating_sub(1)
                    && error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                log::debug!(
                    "Skipping inaccessible image-adjacent nspawn config {}",
                    path.display()
                );
            }
            Err(error) => return Err(NspawnError::Io(path.clone(), error)),
        }
    }
    Ok(None)
}

async fn read_config_at(path: &Path) -> Result<Option<NspawnConfigInspection>> {
    let Some(content) = read_optional(path).await? else {
        return Ok(None);
    };
    validate_content_size(&content)?;
    Ok(Some(NspawnConfigInspection {
        path: path.to_path_buf(),
        content,
    }))
}

async fn read_optional(path: &Path) -> Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_optional_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_config_at(path: &Path) -> Result<()> {
    remove_optional_file(path).await?;
    remove_optional_file(&crate::adapters::filesystem::lock_path_for(path)).await?;
    remove_optional_file(&path.with_extension("lock")).await?;
    Ok(())
}

async fn cleanup_sidecar_locks_at(paths: &[PathBuf]) -> Result<bool> {
    let mut cleaned = false;
    for path in paths {
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
        }

        let lock_path = crate::adapters::filesystem::lock_path_for(path);
        let legacy_lock_path = path.with_extension("lock");
        cleaned |= AsyncLockedWriter::remove_lock_if_target_absent(path, &lock_path).await?;
        cleaned |= AsyncLockedWriter::remove_lock_if_target_absent(path, &legacy_lock_path).await?;
    }
    Ok(cleaned)
}

async fn apply_new_content_at(
    path: &Path,
    content: String,
    mode: Option<u32>,
) -> Result<ApplyStatus> {
    let apply = move |existing: Option<String>| {
        Ok(match existing {
            None => (Some(content), ApplyStatus::Created),
            Some(existing) if existing == content => (None, ApplyStatus::Unchanged),
            Some(_) => (None, ApplyStatus::ConflictUnknownOwner),
        })
    };
    match mode {
        Some(mode) => AsyncLockedWriter::apply_locked_with_mode(path, mode, apply).await,
        None => AsyncLockedWriter::apply_locked(path, apply).await,
    }
}

#[cfg(test)]
async fn promote_oci_config(
    machine: &MachineName,
    network: OciNetworkMode,
    machines_dir: &Path,
    admin_dir: &Path,
    runtime_dir: &Path,
) -> Result<ApplyStatus> {
    promote_oci_config_inner(machine, network, machines_dir, admin_dir, runtime_dir, true).await
}

async fn promote_new_oci_config(
    machine: &MachineName,
    network: OciNetworkMode,
    machines_dir: &Path,
    admin_dir: &Path,
    runtime_dir: &Path,
) -> Result<ApplyStatus> {
    promote_oci_config_inner(
        machine,
        network,
        machines_dir,
        admin_dir,
        runtime_dir,
        false,
    )
    .await
}

async fn promote_oci_config_inner(
    machine: &MachineName,
    network: OciNetworkMode,
    machines_dir: &Path,
    admin_dir: &Path,
    runtime_dir: &Path,
    replace_owned: bool,
) -> Result<ApplyStatus> {
    validate_oci_promotion_target(machine, admin_dir, runtime_dir).await?;
    let filename = format!("{}.nspawn", machine.as_str());
    let source = machines_dir.join(&filename);
    let destination = admin_dir.join(filename);
    let source_content = read_trusted_oci_config(&source).await?;

    let content = promote_oci_content(&source_content, network)?;
    validate_content_size(&content)?;
    AsyncLockedWriter::apply_locked_with_mode(&destination, 0o640, move |existing| {
        Ok(match existing {
            None => (Some(content), ApplyStatus::Created),
            Some(existing) if existing == content => (None, ApplyStatus::Unchanged),
            Some(existing)
                if replace_owned && existing.lines().next() == Some(LASPER_OCI_CONFIG_MARKER) =>
            {
                (Some(content), ApplyStatus::ReplacedOwned)
            }
            Some(_) => (None, ApplyStatus::ConflictUnknownOwner),
        })
    })
    .await
}

async fn validate_oci_promotion_target(
    machine: &MachineName,
    admin_dir: &Path,
    runtime_dir: &Path,
) -> Result<()> {
    let filename = format!("{}.nspawn", machine.as_str());
    let admin = admin_dir.join(&filename);
    if let Some(metadata) = optional_symlink_metadata(&admin).await? {
        if !metadata.file_type().is_file() {
            return Err(NspawnError::Validation(format!(
                "Refusing to replace non-regular administrator configuration: {}",
                admin.display()
            )));
        }
        let existing = tokio::fs::read_to_string(&admin)
            .await
            .map_err(|error| NspawnError::Io(admin.clone(), error))?;
        validate_content_size(&existing)?;
        if existing.lines().next() != Some(LASPER_OCI_CONFIG_MARKER) {
            return Err(NspawnError::Validation(format!(
                "Refusing to replace existing administrator configuration: {}",
                admin.display()
            )));
        }
    }

    let runtime = runtime_dir.join(filename);
    if optional_symlink_metadata(&runtime).await?.is_some() {
        return Err(NspawnError::Validation(format!(
            "Refusing to mask existing runtime configuration: {}",
            runtime.display()
        )));
    }
    Ok(())
}

async fn optional_symlink_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn read_trusted_oci_config(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_trusted_oci_config_blocking(&path))
        .await
        .map_err(|error| NspawnError::Runtime(format!("OCI config reader failed: {error}")))?
}

fn read_trusted_oci_config_blocking(path: &Path) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                NspawnError::Validation(format!(
                    "OCI runtime configuration is not a regular file: {}",
                    path.display()
                ))
            } else {
                NspawnError::Io(path.to_path_buf(), error)
            }
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if !metadata.file_type().is_file() {
        return Err(NspawnError::Validation(format!(
            "OCI runtime configuration is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.uid() != uzers::get_current_uid() || metadata.permissions().mode() & 0o022 != 0 {
        return Err(NspawnError::Validation(format!(
            "OCI runtime configuration is not owned exclusively by the privileged executor: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_NSPAWN_CONTENT_BYTES as u64 {
        return Err(NspawnError::Validation(format!(
            ".nspawn content exceeds {} bytes",
            MAX_NSPAWN_CONTENT_BYTES
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_NSPAWN_CONTENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if bytes.len() > MAX_NSPAWN_CONTENT_BYTES {
        return Err(NspawnError::Validation(format!(
            ".nspawn content exceeds {} bytes",
            MAX_NSPAWN_CONTENT_BYTES
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        NspawnError::Validation(format!(
            "OCI runtime configuration is not valid UTF-8: {error}"
        ))
    })
}

fn promote_oci_content(content: &str, network: OciNetworkMode) -> Result<String> {
    let had_trailing_newline = content.ends_with('\n');
    let mut result = Vec::new();
    let mut in_exec = false;
    let mut in_network = false;
    let mut found_exec = false;
    let mut found_network = false;

    if content.lines().next() != Some(LASPER_OCI_CONFIG_MARKER) {
        result.push(LASPER_OCI_CONFIG_MARKER.to_string());
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_network {
                append_oci_network_policy(&mut result, network);
            }
            in_exec = trimmed == "[Exec]";
            in_network = trimmed == "[Network]";
            if in_exec {
                found_exec = true;
            }
            if in_network {
                found_network = true;
            }
            result.push(line.to_string());
            if in_exec {
                result.push("PrivateUsers=no".to_string());
                result.push(format!("ResolvConf={}", oci_resolv_conf(network)));
            }
            continue;
        }

        let is_private_users = in_exec
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "PrivateUsers");
        let is_resolv_conf = in_exec
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "ResolvConf");
        let is_managed_network_key = in_network
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| oci_managed_network_key(key.trim()));
        if !is_private_users && !is_resolv_conf && !is_managed_network_key {
            result.push(line.to_string());
        }
    }

    if !found_exec {
        return Err(NspawnError::Validation(
            "OCI runtime configuration has no [Exec] section".into(),
        ));
    }

    if in_network {
        append_oci_network_policy(&mut result, network);
    }
    if !found_network {
        if result.last().is_some_and(|line| !line.is_empty()) {
            result.push(String::new());
        }
        result.push("[Network]".to_string());
        append_oci_network_policy(&mut result, network);
    }

    let mut promoted = result.join("\n");
    if had_trailing_newline {
        promoted.push('\n');
    }
    validate_content_size(&promoted)?;
    Ok(promoted)
}

fn oci_resolv_conf(network: OciNetworkMode) -> &'static str {
    match network {
        OciNetworkMode::Host => "bind-host",
        OciNetworkMode::Isolated | OciNetworkMode::Veth => "off",
    }
}

fn append_oci_network_policy(result: &mut Vec<String>, network: OciNetworkMode) {
    match network {
        OciNetworkMode::Host => {
            result.push("Private=no".to_string());
            result.push("VirtualEthernet=no".to_string());
        }
        OciNetworkMode::Isolated => {
            result.push("Private=yes".to_string());
            result.push("VirtualEthernet=no".to_string());
        }
        OciNetworkMode::Veth => {
            result.push("Private=yes".to_string());
            result.push("VirtualEthernet=yes".to_string());
        }
    }
}

fn oci_managed_network_key(key: &str) -> bool {
    matches!(
        key,
        "Private"
            | "VirtualEthernet"
            | "VirtualEthernetExtra"
            | "Interface"
            | "MACVLAN"
            | "IPVLAN"
            | "Bridge"
            | "Zone"
            | "NamespacePath"
            | "Port"
    )
}

async fn write_generated_at(
    path: &Path,
    spec: &NspawnConfigSpec,
    xdg_runtime: Option<&str>,
    nvidia_state: Option<&NvidiaState>,
    invoking_uid: u32,
) -> Result<ApplyStatus> {
    spec.validate()?;
    validate_custom_bind_sources(spec).await?;
    let wayland_socket = validate_wayland_runtime(spec, xdg_runtime, invoking_uid).await?;
    if let Some(state) = nvidia_state {
        validate_nvidia_update(state, &[])?;
    }

    let mut content = nspawn_config_content_from_spec_with_wayland_path(
        spec,
        xdg_runtime,
        wayland_socket.as_deref(),
    )?;
    if let Some(state) = nvidia_state {
        content = NspawnConfig::apply_gpu_passthrough_to_content(content, state, &[])?;
    }
    validate_content_size(&content)?;
    apply_new_content_at(path, content, None).await
}

async fn validate_custom_bind_sources(spec: &NspawnConfigSpec) -> Result<()> {
    for bind in &spec.bind_mounts {
        let source = PathBuf::from(&bind.source);
        match tokio::fs::metadata(&source).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(NspawnError::Validation(format!(
                    "Bind source does not exist: {}",
                    source.display()
                )));
            }
            Err(error) => return Err(NspawnError::Io(source, error)),
        }
    }
    Ok(())
}

async fn update_gpu_at(
    path: &Path,
    machine: MachineName,
    state: NvidiaState,
    removed_binds: Vec<crate::adapters::platform::nvidia::state::PassthroughBind>,
) -> Result<()> {
    validate_nvidia_update(&state, &removed_binds)?;
    AsyncLockedWriter::write_locked(path, move |existing| {
        let content = existing.ok_or_else(|| {
            NspawnError::Runtime(format!("No .nspawn configuration found for {machine}"))
        })?;
        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &state, &removed_binds)?;
        validate_content_size(&updated)?;
        Ok(updated)
    })
    .await
}

fn validate_content_size(content: &str) -> Result<()> {
    if content.len() > MAX_NSPAWN_CONTENT_BYTES {
        return Err(NspawnError::Validation(format!(
            ".nspawn content exceeds {} bytes",
            MAX_NSPAWN_CONTENT_BYTES
        )));
    }
    Ok(())
}

fn validate_nvidia_update(
    state: &NvidiaState,
    removed_binds: &[crate::adapters::platform::nvidia::state::PassthroughBind],
) -> Result<()> {
    if state.binds.len() > MAX_NVIDIA_BINDS || removed_binds.len() > MAX_NVIDIA_BINDS {
        return Err(NspawnError::Validation(
            "Too many NVIDIA bind entries".into(),
        ));
    }

    for bind in &state.binds {
        validate_absolute_value("NVIDIA host path", &bind.host_path)?;
        validate_absolute_value("NVIDIA container path", &bind.container_path)?;
    }
    for bind in removed_binds {
        validate_absolute_value("removed NVIDIA host path", &bind.host_path)?;
        validate_absolute_value("removed NVIDIA container path", &bind.container_path)?;
    }
    Ok(())
}

fn validate_absolute_value(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.chars().any(char::is_control) || !Path::new(value).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "Invalid {label}: {value:?}"
        )));
    }
    Ok(())
}

async fn validate_wayland_runtime(
    spec: &NspawnConfigSpec,
    runtime: Option<&str>,
    invoking_uid: u32,
) -> Result<Option<PathBuf>> {
    let Some(socket_name) = spec.wayland_socket.as_deref() else {
        return Ok(None);
    };
    let runtime = runtime.ok_or_else(|| {
        NspawnError::Validation("Wayland passthrough requires an XDG runtime directory".into())
    })?;
    if runtime.chars().any(char::is_control) || !Path::new(runtime).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "Invalid XDG runtime directory: {runtime:?}"
        )));
    }

    let metadata = tokio::fs::symlink_metadata(runtime)
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(runtime), error))?;
    if !metadata.is_dir() || metadata.uid() != invoking_uid {
        return Err(NspawnError::Validation(format!(
            "XDG runtime directory is not owned by uid {invoking_uid}"
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(NspawnError::Validation(
            "XDG runtime directory is writable by group or others".into(),
        ));
    }

    let requested_socket = Path::new(runtime).join(socket_name);
    let socket_metadata = tokio::fs::symlink_metadata(&requested_socket)
        .await
        .map_err(|error| NspawnError::Io(requested_socket.clone(), error))?;
    if socket_metadata.file_type().is_symlink() {
        let target = tokio::fs::canonicalize(&requested_socket)
            .await
            .map_err(|error| NspawnError::Io(requested_socket.clone(), error))?;
        validate_wayland_socket_target(&target).await?;
        Ok(Some(target))
    } else {
        validate_wayland_socket_target(&requested_socket).await?;
        Ok(Some(requested_socket))
    }
}

async fn validate_wayland_socket_target(path: &Path) -> Result<()> {
    let path_text = path
        .to_str()
        .ok_or_else(|| NspawnError::Validation("Wayland socket path is not valid UTF-8".into()))?;
    if path_text.chars().any(char::is_control) || !path.is_absolute() {
        return Err(NspawnError::Validation(format!(
            "Invalid Wayland socket path: {path_text:?}"
        )));
    }

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if !metadata.file_type().is_socket() {
        return Err(NspawnError::Validation(format!(
            "Wayland path is not a socket: {}",
            path.display()
        )));
    }
    Ok(())
}

fn invoking_uid() -> u32 {
    if uzers::get_current_uid() == 0 {
        if let Ok(uid) = std::env::var("SUDO_UID") {
            if let Ok(uid) = uid.parse() {
                return uid;
            }
        }
    }
    uzers::get_current_uid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::IdmapSuffix;

    #[test]
    fn operation_deserialization_rejects_invalid_names() {
        let managed_read = r#"{
            "operation": "read",
            "params": {"machine": "../escape"}
        }"#;
        let inspection = r#"{
            "operation": "inspect",
            "params": {"image": "../escape"}
        }"#;
        assert!(serde_json::from_str::<NspawnConfigOperation>(managed_read).is_err());
        assert!(serde_json::from_str::<NspawnConfigOperation>(inspection).is_err());
    }

    #[test]
    fn inspect_operation_accepts_image_names_beyond_machine_name_syntax() {
        let json = r#"{
            "operation": "inspect",
            "params": {"image": "vendor image"}
        }"#;
        assert!(serde_json::from_str::<NspawnConfigOperation>(json).is_ok());

        let cleanup = r#"{
            "operation": "cleanup_sidecar_locks",
            "params": {"image": "vendor image"}
        }"#;
        assert!(serde_json::from_str::<NspawnConfigOperation>(cleanup).is_ok());
    }

    #[test]
    fn write_operation_contains_no_account_execution_data() {
        let config = ContainerConfig {
            name: "test".into(),
            users: vec![crate::nspawn::models::CreateUser {
                username: "alice".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let operation = NspawnConfigOperation::Write(WriteNspawnConfig {
            spec: NspawnConfigSpec::try_from(&config).unwrap(),
            xdg_runtime: None,
            nvidia_state: None,
        });
        let json = serde_json::to_string(&operation).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(!json.contains("root_password"));
        assert!(value["params"]["spec"].get("users").is_none());
    }

    #[test]
    fn oci_promotion_operation_accepts_only_supported_network_modes() {
        let host = r#"{
            "operation": "promote_oci",
            "params": {"machine": "web-app", "network": "host"}
        }"#;
        let isolated = r#"{
            "operation": "promote_oci",
            "params": {"machine": "web-app", "network": "isolated"}
        }"#;
        let veth = r#"{
            "operation": "promote_oci",
            "params": {"machine": "web-app", "network": "veth"}
        }"#;
        let caller_selected_userns = r#"{
            "operation": "promote_oci",
            "params": {"machine": "web-app", "network": "host", "private_users": "managed"}
        }"#;
        assert!(serde_json::from_str::<NspawnConfigOperation>(host).is_ok());
        assert!(serde_json::from_str::<NspawnConfigOperation>(isolated).is_ok());
        assert!(serde_json::from_str::<NspawnConfigOperation>(veth).is_ok());
        assert!(serde_json::from_str::<NspawnConfigOperation>(caller_selected_userns).is_err());
    }

    #[test]
    fn oci_content_promotion_preserves_runtime_fields_and_unknown_content() {
        let source = "# Generated from OCI configuration object\n\
[Exec]\n\
Boot=no\n\
KillSignal=TERM\n\
User=1000\n\
WorkingDirectory=/srv/app\n\
Environment=ONE=1\n\
Environment=TWO=two words\n\
Parameters=/usr/bin/example --serve\n\
PrivateUsers=pick\n\
\n\
[Vendor]\n\
Unknown=preserve-me\n";

        let promoted = promote_oci_content(source, OciNetworkMode::Host).unwrap();

        assert!(promoted.starts_with(&format!("{LASPER_OCI_CONFIG_MARKER}\n")));
        assert_eq!(promoted.matches("PrivateUsers=no").count(), 1);
        assert_eq!(promoted.matches("ResolvConf=bind-host").count(), 1);
        assert!(!promoted.contains("PrivateUsers=pick"));
        assert!(promoted.contains("[Network]"));
        assert!(promoted.contains("Private=no"));
        assert!(promoted.contains("VirtualEthernet=no"));
        for preserved in [
            "# Generated from OCI configuration object",
            "Boot=no",
            "KillSignal=TERM",
            "User=1000",
            "WorkingDirectory=/srv/app",
            "Environment=ONE=1",
            "Environment=TWO=two words",
            "Parameters=/usr/bin/example --serve",
            "[Vendor]",
            "Unknown=preserve-me",
        ] {
            assert!(promoted.contains(preserved), "missing {preserved:?}");
        }
        assert!(promoted.ends_with('\n'));
    }

    #[test]
    fn oci_content_promotion_rejects_missing_exec_section() {
        let error = promote_oci_content(
            "# Generated from OCI configuration object\n[Network]\nVirtualEthernet=no\n",
            OciNetworkMode::Host,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NspawnError::Validation(message) if message.contains("no [Exec] section")
        ));
    }

    #[test]
    fn oci_content_promotion_maps_network_modes_to_systemd_settings() {
        let source = "[Exec]\nBoot=no\n\n[Network]\nBridge=br0\nPrivate=no\n";
        for (mode, resolv_conf, private, veth) in [
            (OciNetworkMode::Host, "bind-host", "no", "no"),
            (OciNetworkMode::Isolated, "off", "yes", "no"),
            (OciNetworkMode::Veth, "off", "yes", "yes"),
        ] {
            let promoted = promote_oci_content(source, mode).unwrap();
            let parsed = ini::Ini::load_from_str(&promoted).unwrap();
            assert_eq!(parsed.get_from(Some("Exec"), "PrivateUsers"), Some("no"));
            assert_eq!(
                parsed.get_from(Some("Exec"), "ResolvConf"),
                Some(resolv_conf)
            );
            assert_eq!(parsed.get_from(Some("Network"), "Private"), Some(private));
            assert_eq!(
                parsed.get_from(Some("Network"), "VirtualEthernet"),
                Some(veth)
            );
            assert_eq!(parsed.get_from(Some("Network"), "Bridge"), None);
        }
    }

    #[tokio::test]
    async fn oci_promotion_writes_trusted_copy_and_refreshes_owned_copy() {
        let machines = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        let runtime = admin.path().join("runtime");
        let machine = MachineName::new("web-app").unwrap();
        let source = machines.path().join("web-app.nspawn");
        let destination = admin.path().join("web-app.nspawn");
        tokio::fs::write(
            &source,
            "[Exec]\nBoot=no\nEnvironment=VERSION=one\nParameters=/bin/app\n",
        )
        .await
        .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();

        let first_apply = promote_oci_config(
            &machine,
            OciNetworkMode::Host,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap();
        assert_eq!(first_apply, ApplyStatus::Created);
        let first = tokio::fs::read_to_string(&destination).await.unwrap();
        assert!(first.contains("PrivateUsers=no"));
        assert!(first.contains("ResolvConf=bind-host"));
        assert!(first.contains("Private=no"));
        assert!(first.contains("VirtualEthernet=no"));
        assert!(first.contains("Environment=VERSION=one"));
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert!(crate::adapters::filesystem::lock_path_for(&destination).exists());

        tokio::fs::write(
            &source,
            "[Exec]\nBoot=no\nEnvironment=VERSION=two\nParameters=/bin/app\n",
        )
        .await
        .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        let second_apply = promote_oci_config(
            &machine,
            OciNetworkMode::Veth,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap();
        assert_eq!(second_apply, ApplyStatus::ReplacedOwned);
        let second = tokio::fs::read_to_string(&destination).await.unwrap();
        assert!(second.contains("PrivateUsers=no"));
        assert!(second.contains("ResolvConf=off"));
        assert!(second.contains("Private=yes"));
        assert!(second.contains("VirtualEthernet=yes"));
        assert!(second.contains("Environment=VERSION=two"));
        assert!(!second.contains("VERSION=one"));
    }

    #[tokio::test]
    async fn oci_promotion_refuses_existing_administrator_config() {
        let machines = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        let runtime = admin.path().join("runtime");
        let machine = MachineName::new("web-app").unwrap();
        let source = machines.path().join("web-app.nspawn");
        let destination = admin.path().join("web-app.nspawn");
        tokio::fs::write(&source, "[Exec]\nBoot=no\nParameters=/bin/app\n")
            .await
            .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        tokio::fs::write(&destination, "[Exec]\nPrivateUsers=no\n")
            .await
            .unwrap();

        let error = promote_oci_config(
            &machine,
            OciNetworkMode::Host,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            NspawnError::Validation(message)
                if message.contains("Refusing to replace existing administrator configuration")
        ));
        assert_eq!(
            tokio::fs::read_to_string(&destination).await.unwrap(),
            "[Exec]\nPrivateUsers=no\n"
        );
    }

    #[tokio::test]
    async fn oci_promotion_does_not_accept_marker_outside_first_line() {
        let machines = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        let runtime = admin.path().join("runtime");
        let machine = MachineName::new("web-app").unwrap();
        let source = machines.path().join("web-app.nspawn");
        let destination = admin.path().join("web-app.nspawn");
        tokio::fs::write(&source, "[Exec]\nBoot=no\nParameters=/bin/app\n")
            .await
            .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        let existing = format!("# administrator file\n{LASPER_OCI_CONFIG_MARKER}\n[Exec]\n");
        tokio::fs::write(&destination, &existing).await.unwrap();

        let error = promote_oci_config(
            &machine,
            OciNetworkMode::Host,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, NspawnError::Validation(_)));
        assert_eq!(
            tokio::fs::read_to_string(&destination).await.unwrap(),
            existing
        );
    }

    #[tokio::test]
    async fn oci_promotion_rejects_symlink_source() {
        let machines = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        let runtime = admin.path().join("runtime");
        let machine = MachineName::new("web-app").unwrap();
        let target = machines.path().join("target.nspawn");
        let source = machines.path().join("web-app.nspawn");
        tokio::fs::write(&target, "[Exec]\nBoot=no\nParameters=/bin/app\n")
            .await
            .unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();

        let error = promote_oci_config(
            &machine,
            OciNetworkMode::Host,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            NspawnError::Validation(message) if message.contains("not a regular file")
        ));
        assert!(!admin.path().join("web-app.nspawn").exists());
    }

    #[tokio::test]
    async fn oci_promotion_rejects_group_writable_source() {
        let machines = tempfile::tempdir().unwrap();
        let admin = tempfile::tempdir().unwrap();
        let runtime = admin.path().join("runtime");
        let machine = MachineName::new("web-app").unwrap();
        let source = machines.path().join("web-app.nspawn");
        tokio::fs::write(&source, "[Exec]\nBoot=no\nParameters=/bin/app\n")
            .await
            .unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o664)).unwrap();

        let error = promote_oci_config(
            &machine,
            OciNetworkMode::Host,
            machines.path(),
            admin.path(),
            &runtime,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            NspawnError::Validation(message)
                if message.contains("not owned exclusively by the privileged executor")
        ));
        assert!(!admin.path().join("web-app.nspawn").exists());
    }

    #[tokio::test]
    async fn oci_promotion_preflight_rejects_runtime_config() {
        let admin = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let machine = MachineName::new("web-app").unwrap();
        let runtime_config = runtime.path().join("web-app.nspawn");
        tokio::fs::write(&runtime_config, "[Exec]\nPrivateUsers=no\n")
            .await
            .unwrap();

        let error = validate_oci_promotion_target(&machine, admin.path(), runtime.path())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NspawnError::Validation(message) if message.contains("existing runtime configuration")
        ));
    }

    #[tokio::test]
    async fn generated_write_is_typed_atomic_and_uses_persistent_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.nspawn");
        let config = ContainerConfig {
            name: "test".into(),
            hostname: "test-host".into(),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let created = write_generated_at(&path, &spec, None, None, uzers::get_current_uid())
            .await
            .unwrap();
        assert_eq!(created, ApplyStatus::Created);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Boot=yes"));
        assert!(content.contains("Hostname=test-host"));
        assert!(crate::adapters::filesystem::lock_path_for(&path).exists());

        let unchanged = write_generated_at(&path, &spec, None, None, uzers::get_current_uid())
            .await
            .unwrap();
        assert_eq!(unchanged, ApplyStatus::Unchanged);

        let different = NspawnConfigSpec::try_from(&ContainerConfig {
            name: "test".into(),
            hostname: "different".into(),
            ..Default::default()
        })
        .unwrap();
        let conflict = write_generated_at(&path, &different, None, None, uzers::get_current_uid())
            .await
            .unwrap();
        assert_eq!(conflict, ApplyStatus::ConflictUnknownOwner);
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), content);
    }

    #[tokio::test]
    async fn config_discovery_prefers_admin_then_runtime_then_image_adjacent() {
        let directory = tempfile::tempdir().unwrap();
        let admin = directory.path().join("etc/test.nspawn");
        let runtime = directory.path().join("run/test.nspawn");
        let image = directory.path().join("machines/test.nspawn");
        for path in [&admin, &runtime, &image] {
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
        }
        tokio::fs::write(&runtime, "[Exec]\nBoot=yes\n")
            .await
            .unwrap();
        tokio::fs::write(&image, "[Exec]\nBoot=no\n").await.unwrap();

        let discovered =
            read_discovered_config_from(&[admin.clone(), runtime.clone(), image.clone()])
                .await
                .unwrap()
                .unwrap();
        assert_eq!(discovered.path, runtime);
        assert!(discovered.content.contains("Boot=yes"));

        tokio::fs::write(&admin, "[Exec]\nPrivateUsers=managed\n")
            .await
            .unwrap();
        let discovered = read_discovered_config_from(&[admin.clone(), runtime, image])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(discovered.path, admin);
        assert!(discovered.content.contains("PrivateUsers=managed"));
    }

    #[tokio::test]
    async fn generated_write_rejects_missing_custom_bind_source() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("test.nspawn");
        let missing = directory.path().join("missing");
        let config = ContainerConfig {
            name: "test".into(),
            bind_mounts: vec![crate::nspawn::models::BindMount {
                source: missing.to_string_lossy().into_owned(),
                target: "/srv/data".into(),
                readonly: false,
                suffix: IdmapSuffix::None,
            }],
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let error = write_generated_at(&output, &spec, None, None, uzers::get_current_uid())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NspawnError::Validation(message)
                if message.contains("Bind source does not exist")
                    && message.contains(missing.to_str().unwrap())
        ));
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn generated_write_allows_custom_bind_source_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let symlink = directory.path().join("source-link");
        let output = directory.path().join("test.nspawn");
        tokio::fs::write(&source, "data").await.unwrap();
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        let config = ContainerConfig {
            name: "test".into(),
            private_users: Some(crate::nspawn::models::PrivateUsersMode::Yes),
            bind_mounts: vec![crate::nspawn::models::BindMount {
                source: symlink.to_string_lossy().into_owned(),
                target: "/srv/data".into(),
                readonly: true,
                suffix: IdmapSuffix::Noidmap,
            }],
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        write_generated_at(&output, &spec, None, None, uzers::get_current_uid())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&output).await.unwrap();
        assert!(content.contains(&format!(
            "BindReadOnly={}:/srv/data:noidmap",
            symlink.display()
        )));
    }

    #[tokio::test]
    async fn remove_config_deletes_config_and_lock_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.nspawn");
        let lock_path = crate::adapters::filesystem::lock_path_for(&path);
        let legacy_lock_path = path.with_extension("lock");
        tokio::fs::write(&path, "[Exec]\nBoot=yes\n").await.unwrap();
        tokio::fs::write(&lock_path, "").await.unwrap();
        tokio::fs::write(&legacy_lock_path, "").await.unwrap();

        remove_config_at(&path).await.unwrap();

        assert!(!path.exists());
        assert!(!lock_path.exists());
        assert!(!legacy_lock_path.exists());
    }

    #[tokio::test]
    async fn cleanup_sidecar_locks_waits_for_config_removal_and_supports_spaces() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vendor image.nspawn");
        let lock_path = crate::adapters::filesystem::lock_path_for(&path);
        let legacy_lock_path = path.with_extension("lock");
        tokio::fs::write(&path, "[Exec]\nBoot=no\n").await.unwrap();
        tokio::fs::write(&lock_path, "").await.unwrap();
        tokio::fs::write(&legacy_lock_path, "").await.unwrap();

        assert!(!cleanup_sidecar_locks_at(std::slice::from_ref(&path))
            .await
            .unwrap());
        assert!(lock_path.exists());
        assert!(legacy_lock_path.exists());

        tokio::fs::remove_file(&path).await.unwrap();
        assert!(cleanup_sidecar_locks_at(std::slice::from_ref(&path))
            .await
            .unwrap());
        assert!(!lock_path.exists());
        assert!(!legacy_lock_path.exists());
        assert!(!cleanup_sidecar_locks_at(std::slice::from_ref(&path))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn gpu_update_preserves_existing_unmanaged_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.nspawn");
        tokio::fs::write(&path, "[Exec]\nBoot=yes\n\n[Custom]\nPreserve=this\n")
            .await
            .unwrap();

        let state = NvidiaState {
            binds: vec![crate::adapters::platform::nvidia::state::PassthroughBind {
                host_path: "/host/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        update_gpu_at(&path, MachineName::new("test").unwrap(), state, Vec::new())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Preserve=this"));
        assert!(content.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
        assert!(content.contains("BindReadOnly=/host/libcuda.so:/usr/lib/libcuda.so"));
    }

    #[tokio::test]
    async fn wayland_runtime_requires_private_user_runtime_and_socket() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.path().join("wayland-0");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let config = ContainerConfig {
            name: "test".into(),
            wayland_socket: Some("wayland-0".into()),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let resolved =
            validate_wayland_runtime(&spec, directory.path().to_str(), uzers::get_current_uid())
                .await
                .unwrap()
                .unwrap();

        assert_eq!(resolved, socket_path);
    }

    #[tokio::test]
    async fn wayland_runtime_allows_leaf_symlink_to_socket() {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let target_socket = target_dir.path().join("wayland-real");
        let _listener = std::os::unix::net::UnixListener::bind(&target_socket).unwrap();
        let symlink_path = runtime.path().join("wayland-0");
        std::os::unix::fs::symlink(&target_socket, &symlink_path).unwrap();

        let config = ContainerConfig {
            name: "test".into(),
            wayland_socket: Some("wayland-0".into()),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let resolved =
            validate_wayland_runtime(&spec, runtime.path().to_str(), uzers::get_current_uid())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            resolved,
            tokio::fs::canonicalize(&target_socket).await.unwrap()
        );

        let path = runtime.path().join("test.nspawn");
        write_generated_at(
            &path,
            &spec,
            runtime.path().to_str(),
            None,
            uzers::get_current_uid(),
        )
        .await
        .unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains(&format!(
            "Bind={}:/mnt/wayland-socket",
            resolved.to_str().unwrap()
        )));
        assert!(!content.contains(symlink_path.to_str().unwrap()));
    }

    #[tokio::test]
    async fn wayland_runtime_rejects_leaf_symlink_to_regular_file() {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let target_file = target_dir.path().join("not-a-socket");
        std::fs::write(&target_file, b"not a socket").unwrap();
        std::os::unix::fs::symlink(&target_file, runtime.path().join("wayland-0")).unwrap();

        let config = ContainerConfig {
            name: "test".into(),
            wayland_socket: Some("wayland-0".into()),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let result =
            validate_wayland_runtime(&spec, runtime.path().to_str(), uzers::get_current_uid())
                .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn wayland_runtime_rejects_world_writable_directory() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("wayland-0");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let config = ContainerConfig {
            name: "test".into(),
            wayland_socket: Some("wayland-0".into()),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&config).unwrap();

        let result =
            validate_wayland_runtime(&spec, directory.path().to_str(), uzers::get_current_uid())
                .await;

        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
    }
}
