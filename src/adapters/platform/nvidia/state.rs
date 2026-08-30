use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::platform::nvidia::classify::{ClassifiedEntry, SymlinkEntry};
use crate::adapters::trusted_state::{StateDirectory, TrustedDirectory, TrustedStateRoot};
use crate::application::image_lifecycle::ArtifactOwnership;
use crate::application::provisioning::ResourceApplyStatus;
use crate::domain::machine::MachineName;
use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

const MAX_NVIDIA_STATE_BYTES: usize = 1024 * 1024;
const MAX_NVIDIA_STATE_ITEMS: usize = 16384;
const NVIDIA_STATE_MARKER: &str = "lasper-nvidia-state-v1";

/// A single host→container path mapping for an nspawn Bind= or BindReadOnly= entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PassthroughBind {
    pub host_path: String,
    pub container_path: String,
    /// false = Bind (rw, device nodes), true = BindReadOnly (ro, libraries/configs)
    pub readonly: bool,
}

/// Hardware and driver information detected on the host for mounting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NvidiaState {
    /// Host driver version when this state was captured.
    pub driver_version: String,

    /// Present only in state files written by the current Lasper cleanup path.
    /// Missing or unknown markers make an existing file ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_marker: Option<String>,

    /// Unified bind-mount entries. This is the primary field.
    #[serde(default)]
    pub binds: Vec<PassthroughBind>,

    // Legacy fields — kept for backward compat with old state files.
    // New state files omit these via skip_serializing_if.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_binds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_binds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classified_entries: Vec<ClassifiedEntry>,

    #[allow(dead_code)]
    #[serde(default)]
    pub symlinks: Vec<SymlinkEntry>,
    #[serde(default)]
    pub ldcache_folders: Vec<String>,
    #[serde(default)]
    pub env_vars: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
}

impl NvidiaState {
    /// Populate `binds` from legacy fields if binds is empty and legacy fields have data.
    /// Called after deserialization of old-format state files.
    pub fn migrate_from_legacy(&mut self) {
        if !self.binds.is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        for path in &self.device_binds {
            let (host, container) = split_host_container(path);
            let key = format!("{}::{}", host, container);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: host,
                    container_path: container,
                    readonly: false,
                });
            }
        }
        for path in &self.readonly_binds {
            let (host, container) = split_host_container(path);
            let key = format!("{}::{}", host, container);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: host,
                    container_path: container,
                    readonly: true,
                });
            }
        }
        for ce in &self.classified_entries {
            let key = format!("{}::{}", ce.host_path, ce.default_container_path);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: ce.host_path.clone(),
                    container_path: ce.default_container_path.clone(),
                    readonly: ce.readonly,
                });
            }
        }
        for sym in &self.symlinks {
            let key = format!("{}::{}", sym.target, sym.link_path);
            if seen.insert(key) {
                self.binds.push(PassthroughBind {
                    host_path: sym.target.clone(),
                    container_path: sym.link_path.clone(),
                    readonly: true,
                });
            }
        }
    }

    /// Extract legacy fields from `binds` for backward compat consumers.
    pub fn populate_legacy(&mut self) {
        self.readonly_binds.clear();
        self.device_binds.clear();
        // classified_entries and symlinks are managed separately
        // (extract_classified_entries), do not clear them here

        for bind in &self.binds {
            if bind.readonly {
                if bind.host_path == bind.container_path {
                    self.readonly_binds.push(bind.host_path.clone());
                } else {
                    self.readonly_binds
                        .push(format!("{}:{}", bind.host_path, bind.container_path));
                }
            } else {
                self.device_binds.push(bind.host_path.clone());
            }
        }
    }

    /// Returns all host paths involved in this state.
    #[allow(dead_code)]
    pub fn all_host_paths(&self) -> Vec<String> {
        let mut paths = HashSet::new();
        for b in &self.binds {
            paths.insert(b.host_path.clone());
        }
        // Also check legacy for code that hasn't migrated yet
        for b in &self.readonly_binds {
            paths.insert(b.split(':').next().unwrap_or(b).to_string());
        }
        for b in &self.device_binds {
            paths.insert(b.split(':').next().unwrap_or(b).to_string());
        }
        for e in &self.classified_entries {
            paths.insert(e.host_path.clone());
        }
        paths.into_iter().collect()
    }

    /// Returns all container paths that were created/mounted.
    pub fn all_container_paths(&self) -> Vec<String> {
        let mut paths = HashSet::new();
        for b in &self.binds {
            paths.insert(b.container_path.clone());
        }
        // Also check legacy for backward compat
        for b in &self.readonly_binds {
            paths.insert(b.split(':').next_back().unwrap_or(b).to_string());
        }
        for b in &self.device_binds {
            paths.insert(b.split(':').next_back().unwrap_or(b).to_string());
        }
        for e in &self.classified_entries {
            paths.insert(e.default_container_path.clone());
        }
        for s in &self.symlinks {
            paths.insert(s.link_path.clone());
        }
        paths.into_iter().collect()
    }

    /// Backward compatibility for legacy code that expects flattened paths.
    pub fn all_paths(&self) -> Vec<String> {
        self.all_container_paths()
    }
}

/// Splits "host:container" or "path" into (host, container).
fn split_host_container(s: &str) -> (String, String) {
    s.split_once(':')
        .map(|(h, c)| (h.to_string(), c.to_string()))
        .unwrap_or((s.to_string(), s.to_string()))
}

/// Typed access to Lasper-managed NVIDIA state files.
#[derive(Clone)]
pub struct NvidiaStateStore {
    executor: Arc<dyn NvidiaStateExecutor>,
}

impl NvidiaStateStore {
    pub(crate) fn direct(state_root: TrustedStateRoot) -> Self {
        Self {
            executor: Arc::new(DirectNvidiaStateExecutor { state_root }),
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            executor: Arc::new(ElevatedNvidiaStateExecutor { daemon }),
        }
    }

    pub async fn read(&self, name: &str) -> Result<Option<NvidiaState>> {
        let result = self
            .execute(NvidiaStateOperation::Read(ReadNvidiaState {
                machine: parse_machine_name(name)?,
            }))
            .await?;
        Ok(result.state)
    }

    pub(crate) async fn probe_owned(&self, name: &str) -> Result<ArtifactOwnership> {
        let result = self
            .execute(NvidiaStateOperation::ProbeOwned(ReadNvidiaState {
                machine: parse_machine_name(name)?,
            }))
            .await?;
        result.ownership.ok_or_else(|| {
            NspawnError::Runtime("NVIDIA state probe returned no ownership evidence".into())
        })
    }

    pub async fn write(&self, name: &str, state: &NvidiaState) -> Result<()> {
        self.execute(NvidiaStateOperation::Write(Box::new(WriteNvidiaState {
            machine: parse_machine_name(name)?,
            state: state.clone(),
        })))
        .await?;
        Ok(())
    }

    pub async fn write_initial(
        &self,
        name: &str,
        state: &NvidiaState,
    ) -> Result<ResourceApplyStatus> {
        let result = self
            .execute(NvidiaStateOperation::WriteInitial(Box::new(
                WriteNvidiaState {
                    machine: parse_machine_name(name)?,
                    state: state.clone(),
                },
            )))
            .await?;
        result.apply.ok_or_else(|| {
            NspawnError::Runtime("initial NVIDIA state write returned no apply status".into())
        })
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        self.execute(NvidiaStateOperation::Remove(RemoveNvidiaState {
            machine: parse_machine_name(name)?,
        }))
        .await?;
        Ok(())
    }

    pub async fn remove_owned(&self, name: &str) -> Result<ArtifactOwnership> {
        let result = self
            .execute(NvidiaStateOperation::RemoveOwned(RemoveNvidiaState {
                machine: parse_machine_name(name)?,
            }))
            .await?;
        result.ownership.ok_or_else(|| {
            NspawnError::Runtime("owned NVIDIA cleanup returned no ownership evidence".into())
        })
    }

    async fn execute(&self, operation: NvidiaStateOperation) -> Result<NvidiaStateResult> {
        self.executor.execute(operation).await
    }
}

impl std::fmt::Debug for NvidiaStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaStateStore")
            .field("route", &self.executor.route())
            .finish()
    }
}

#[async_trait::async_trait]
trait NvidiaStateExecutor: Send + Sync + 'static {
    fn route(&self) -> &'static str;

    async fn execute(&self, operation: NvidiaStateOperation) -> Result<NvidiaStateResult>;
}

struct DirectNvidiaStateExecutor {
    state_root: TrustedStateRoot,
}

#[async_trait::async_trait]
impl NvidiaStateExecutor for DirectNvidiaStateExecutor {
    fn route(&self) -> &'static str {
        "direct"
    }

    async fn execute(&self, operation: NvidiaStateOperation) -> Result<NvidiaStateResult> {
        execute_nvidia_state_operation(operation, self.state_root.clone()).await
    }
}

struct ElevatedNvidiaStateExecutor {
    daemon: Arc<ElevatedDaemon>,
}

#[async_trait::async_trait]
impl NvidiaStateExecutor for ElevatedNvidiaStateExecutor {
    fn route(&self) -> &'static str {
        "elevated_rpc"
    }

    async fn execute(&self, operation: NvidiaStateOperation) -> Result<NvidiaStateResult> {
        self.daemon
            .nvidia_state(operation)
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum NvidiaStateOperation {
    Read(ReadNvidiaState),
    ProbeOwned(ReadNvidiaState),
    Write(Box<WriteNvidiaState>),
    WriteInitial(Box<WriteNvidiaState>),
    Remove(RemoveNvidiaState),
    RemoveOwned(RemoveNvidiaState),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadNvidiaState {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteNvidiaState {
    machine: MachineName,
    state: NvidiaState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveNvidiaState {
    machine: MachineName,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NvidiaStateResult {
    state: Option<NvidiaState>,
    #[serde(default)]
    apply: Option<ResourceApplyStatus>,
    #[serde(default)]
    ownership: Option<ArtifactOwnership>,
}

pub(crate) async fn execute_nvidia_state_operation(
    operation: NvidiaStateOperation,
    state_root: TrustedStateRoot,
) -> Result<NvidiaStateResult> {
    tokio::task::spawn_blocking(move || {
        execute_nvidia_state_operation_blocking(operation, state_root)
    })
    .await
    .map_err(|error| NspawnError::Runtime(format!("NVIDIA state task failed: {error}")))?
}

fn execute_nvidia_state_operation_blocking(
    operation: NvidiaStateOperation,
    state_root: TrustedStateRoot,
) -> Result<NvidiaStateResult> {
    let directory = state_root.directory(StateDirectory::States)?;
    match operation {
        NvidiaStateOperation::Read(request) => Ok(NvidiaStateResult {
            state: read_state(&directory, &state_file_name(&request.machine))?,
            ..Default::default()
        }),
        NvidiaStateOperation::ProbeOwned(request) => Ok(NvidiaStateResult {
            ownership: Some(probe_owned_state(
                &directory,
                &state_file_name(&request.machine),
            )?),
            ..Default::default()
        }),
        NvidiaStateOperation::Write(request) => {
            write_state(
                &directory,
                &state_file_name(&request.machine),
                &request.state,
            )?;
            Ok(NvidiaStateResult::default())
        }
        NvidiaStateOperation::WriteInitial(request) => {
            let apply = write_initial_state(
                &directory,
                &state_file_name(&request.machine),
                &request.state,
            )?;
            Ok(NvidiaStateResult {
                apply: Some(apply),
                ..Default::default()
            })
        }
        NvidiaStateOperation::Remove(request) => {
            directory.remove_with_lock(&state_file_name(&request.machine))?;
            Ok(NvidiaStateResult::default())
        }
        NvidiaStateOperation::RemoveOwned(request) => {
            let ownership = remove_owned_state(&directory, &state_file_name(&request.machine))?;
            Ok(NvidiaStateResult {
                ownership: Some(ownership),
                ..Default::default()
            })
        }
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn state_file_name(machine: &MachineName) -> String {
    format!("{}.json", machine.as_str())
}

fn read_state(directory: &TrustedDirectory, name: &str) -> Result<Option<NvidiaState>> {
    let Some(file) = directory.read_bounded(name, MAX_NVIDIA_STATE_BYTES)? else {
        return Ok(None);
    };
    if file.uid != directory.expected_uid() || file.mode != 0o600 {
        return Err(NspawnError::Validation(format!(
            "NVIDIA state has unsafe ownership or mode: {name}"
        )));
    }
    let mut state: NvidiaState = serde_json::from_slice(&file.bytes)?;
    state.migrate_from_legacy();
    validate_state(&state)?;
    Ok(Some(state))
}

fn write_state(directory: &TrustedDirectory, name: &str, state: &NvidiaState) -> Result<()> {
    validate_state(state)?;
    let mut state = state.clone();
    state.ownership_marker = Some(NVIDIA_STATE_MARKER.to_string());
    let content = serde_json::to_vec_pretty(&state)?;
    if content.len() > MAX_NVIDIA_STATE_BYTES {
        return Err(NspawnError::Validation(format!(
            "NVIDIA state exceeds {} bytes",
            MAX_NVIDIA_STATE_BYTES
        )));
    }
    directory.with_exclusive_lock(name, || {
        if let Some(existing) = directory.read_bounded(name, MAX_NVIDIA_STATE_BYTES)? {
            if existing.uid != directory.expected_uid() || existing.mode != 0o600 {
                return Err(NspawnError::Validation(format!(
                    "Refusing to replace NVIDIA state with unsafe ownership or mode: {name}"
                )));
            }
        }
        directory.write_atomic(name, &content, 0o600)
    })
}

fn write_initial_state(
    directory: &TrustedDirectory,
    name: &str,
    state: &NvidiaState,
) -> Result<ResourceApplyStatus> {
    validate_state(state)?;
    let mut state = state.clone();
    state.ownership_marker = Some(NVIDIA_STATE_MARKER.to_string());
    let content = serde_json::to_vec_pretty(&state)?;
    if content.len() > MAX_NVIDIA_STATE_BYTES {
        return Err(NspawnError::Validation(format!(
            "NVIDIA state exceeds {} bytes",
            MAX_NVIDIA_STATE_BYTES
        )));
    }
    directory.with_exclusive_lock(name, || {
        let ownership = probe_owned_state(directory, name)?;
        let existing = directory.read_bounded(name, MAX_NVIDIA_STATE_BYTES)?;
        match existing {
            None => {
                directory.write_atomic(name, &content, 0o600)?;
                Ok(ResourceApplyStatus::Created)
            }
            Some(existing) if existing.bytes == content => Ok(ResourceApplyStatus::Unchanged),
            Some(_existing) if ownership == ArtifactOwnership::ProvenOwned => {
                directory.write_atomic(name, &content, 0o600)?;
                Ok(ResourceApplyStatus::ReplacedOwned)
            }
            Some(_) => Ok(ResourceApplyStatus::ConflictUnknownOwner),
        }
    })
}

fn remove_owned_state(directory: &TrustedDirectory, name: &str) -> Result<ArtifactOwnership> {
    directory.with_exclusive_lock_and_cleanup(name, || {
        let ownership = probe_owned_state(directory, name)?;
        if ownership == ArtifactOwnership::ProvenOwned {
            directory.remove_unlocked(name)?;
        }
        Ok(ownership)
    })
}

fn probe_owned_state(directory: &TrustedDirectory, name: &str) -> Result<ArtifactOwnership> {
    let file = match directory.read_bounded(name, MAX_NVIDIA_STATE_BYTES) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(ArtifactOwnership::NotPresent),
        Err(NspawnError::Validation(_)) => return Ok(ArtifactOwnership::AmbiguousLegacy),
        Err(error) => return Err(error),
    };
    if file.uid != directory.expected_uid() || file.mode != 0o600 {
        return Ok(ArtifactOwnership::AmbiguousLegacy);
    }
    Ok(if is_owned_state_content(&file.bytes) {
        ArtifactOwnership::ProvenOwned
    } else {
        ArtifactOwnership::AmbiguousLegacy
    })
}

fn is_owned_state_content(content: &[u8]) -> bool {
    if content.len() > MAX_NVIDIA_STATE_BYTES {
        return false;
    }
    let Ok(state) = serde_json::from_slice::<NvidiaState>(content) else {
        return false;
    };
    state.ownership_marker.as_deref() == Some(NVIDIA_STATE_MARKER) && validate_state(&state).is_ok()
}

fn validate_state(state: &NvidiaState) -> Result<()> {
    if let Some(marker) = &state.ownership_marker {
        if marker != NVIDIA_STATE_MARKER {
            return Err(NspawnError::Validation(
                "unknown NVIDIA state ownership marker".into(),
            ));
        }
    }
    let total_items = state.binds.len()
        + state.readonly_binds.len()
        + state.device_binds.len()
        + state.classified_entries.len()
        + state.symlinks.len()
        + state.ldcache_folders.len()
        + state.env_vars.len()
        + state
            .profile
            .as_ref()
            .map(|profile| {
                profile.category_destinations.len() + profile.manual_classifications.len()
            })
            .unwrap_or_default();
    if total_items > MAX_NVIDIA_STATE_ITEMS {
        return Err(NspawnError::Validation(
            "Too many entries in NVIDIA state".into(),
        ));
    }

    validate_text_value("driver version", &state.driver_version, true)?;
    for bind in &state.binds {
        validate_absolute_value("NVIDIA host path", &bind.host_path)?;
        validate_absolute_value("NVIDIA container path", &bind.container_path)?;
    }
    for bind in &state.readonly_binds {
        validate_bind_expression("legacy read-only bind", bind)?;
    }
    for bind in &state.device_binds {
        validate_bind_expression("legacy device bind", bind)?;
    }
    for entry in &state.classified_entries {
        validate_absolute_value("classified host path", &entry.host_path)?;
        validate_absolute_value("classified container path", &entry.default_container_path)?;
    }
    for symlink in &state.symlinks {
        validate_absolute_value("symlink target", &symlink.target)?;
        validate_absolute_value("symlink path", &symlink.link_path)?;
    }
    for folder in &state.ldcache_folders {
        validate_absolute_value("ldcache folder", folder)?;
    }
    for (key, value) in &state.env_vars {
        validate_env_key(key)?;
        validate_text_value("environment value", value, true)?;
    }
    if let Some(profile) = &state.profile {
        validate_text_value("GPU device selector", &profile.gpu_device, false)?;
        for destination in profile.category_destinations.values() {
            validate_absolute_value("NVIDIA category destination", destination)?;
        }
        for classification in &profile.manual_classifications {
            validate_absolute_value("manual classification host path", &classification.host_path)?;
            if !classification.destination.is_empty() {
                validate_absolute_value(
                    "manual classification destination",
                    &classification.destination,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_bind_expression(label: &str, value: &str) -> Result<()> {
    validate_text_value(label, value, false)?;
    let source = value.split_once(':').map_or(value, |(source, _)| source);
    if !Path::new(source).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "{label} source must be absolute: {value:?}"
        )));
    }
    Ok(())
}

fn validate_absolute_value(label: &str, value: &str) -> Result<()> {
    validate_text_value(label, value, false)?;
    if !Path::new(value).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "{label} must be absolute: {value:?}"
        )));
    }
    Ok(())
}

fn validate_env_key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.contains('=')
        || value.chars().any(char::is_control)
    {
        return Err(NspawnError::Validation(format!(
            "Invalid environment key: {value:?}"
        )));
    }
    Ok(())
}

fn validate_text_value(label: &str, value: &str, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.is_empty())
        || value.len() > 4096
        || value.chars().any(char::is_control)
    {
        return Err(NspawnError::Validation(format!(
            "Invalid {label}: {value:?}"
        )));
    }
    Ok(())
}

pub(crate) fn calculate_death_list(old: &NvidiaState, new: &NvidiaState) -> Vec<String> {
    let old_paths = old.all_paths();
    let new_paths = new.all_paths();
    old_paths
        .into_iter()
        .filter(|p| !new_paths.contains(p))
        .collect()
}

pub(crate) fn calculate_removed_binds(
    old: &NvidiaState,
    new: &NvidiaState,
) -> Vec<PassthroughBind> {
    let new_binds = new.binds.iter().collect::<HashSet<_>>();
    old.binds
        .iter()
        .filter(|bind| !new_binds.contains(bind))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubNvidiaStateExecutor;

    #[async_trait::async_trait]
    impl NvidiaStateExecutor for StubNvidiaStateExecutor {
        fn route(&self) -> &'static str {
            "test"
        }

        async fn execute(&self, operation: NvidiaStateOperation) -> Result<NvidiaStateResult> {
            assert!(matches!(operation, NvidiaStateOperation::Read(_)));
            Ok(NvidiaStateResult {
                state: Some(NvidiaState {
                    driver_version: "test-driver".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn store_delegates_to_its_fixed_executor() {
        let store = NvidiaStateStore {
            executor: Arc::new(StubNvidiaStateExecutor),
        };

        let state = store.read("test").await.unwrap().unwrap();

        assert_eq!(state.driver_version, "test-driver");
        assert!(format!("{store:?}").contains("test"));
    }
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_passthrough_bind_serde_roundtrip() {
        let bind = PassthroughBind {
            host_path: "/host/lib/libcuda.so".into(),
            container_path: "/usr/lib/libcuda.so".into(),
            readonly: true,
        };
        let json = serde_json::to_string(&bind).unwrap();
        let back: PassthroughBind = serde_json::from_str(&json).unwrap();
        assert_eq!(bind, back);
    }

    #[test]
    fn test_migrate_empty_state_noop() {
        let mut state = NvidiaState::default();
        state.migrate_from_legacy();
        assert!(state.binds.is_empty());
    }

    #[test]
    fn test_migrate_already_populated_noop() {
        let mut state = NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/host/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            readonly_binds: vec!["/old/path".into()],
            ..Default::default()
        };
        state.migrate_from_legacy();
        assert_eq!(state.binds.len(), 1);
    }

    #[test]
    fn test_migrate_from_legacy_all_fields() {
        let mut state = NvidiaState {
            driver_version: "1.0".into(),
            device_binds: vec!["/dev/nvidia0".into()],
            readonly_binds: vec!["/usr/lib/libcuda.so:/usr/lib/libcuda.so".into()],
            classified_entries: vec![ClassifiedEntry {
                host_path: "/host/gsp.bin".into(),
                default_container_path: "/lib/firmware/nvidia/gsp.bin".into(),
                category: crate::domain::nvidia::NvidiaFileCategory::Firmware,
                readonly: true,
            }],
            symlinks: vec![SymlinkEntry {
                target: "/usr/lib/libcuda.so.1".into(),
                link_path: "/usr/lib/libcuda.so".into(),
            }],
            ..Default::default()
        };
        state.migrate_from_legacy();

        // device_binds → readonly:false
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/dev/nvidia0" && !b.readonly));
        // readonly_binds with host:container
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so"
                && b.container_path == "/usr/lib/libcuda.so"
                && b.readonly));
        // classified_entries
        assert!(state.binds.iter().any(|b| b.host_path == "/host/gsp.bin"
            && b.container_path == "/lib/firmware/nvidia/gsp.bin"));
        // symlinks
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so.1"
                && b.container_path == "/usr/lib/libcuda.so"
                && b.readonly));
    }

    #[test]
    fn test_migrate_dedup_same_path() {
        let mut state = NvidiaState {
            device_binds: vec!["/dev/nvidia0".into()],
            readonly_binds: vec!["/dev/nvidia0".into()], // same host path as device
            ..Default::default()
        };
        state.migrate_from_legacy();
        // Only one entry per unique (host, container) pair
        let dev_entries: Vec<_> = state
            .binds
            .iter()
            .filter(|b| b.host_path == "/dev/nvidia0")
            .collect();
        assert_eq!(dev_entries.len(), 1);
    }

    #[test]
    fn test_populate_legacy_from_binds() {
        let state = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };
        let mut clone = state.clone();
        clone.populate_legacy();
        assert!(clone.device_binds.contains(&"/dev/nvidia0".to_string()));
        assert!(clone
            .readonly_binds
            .contains(&"/host/libcuda.so:/usr/lib/libcuda.so".to_string()));
    }

    #[test]
    fn test_all_paths_from_binds() {
        let state = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };
        let paths = state.all_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/dev/nvidia0".to_string()));
        assert!(paths.contains(&"/usr/lib/libcuda.so".to_string()));
    }

    #[test]
    fn test_all_paths_empty_state() {
        let state = NvidiaState::default();
        assert!(state.all_paths().is_empty());
    }

    #[test]
    fn test_calculate_death_list_removed_paths() {
        let old = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![
                PassthroughBind {
                    host_path: "/ro1".into(),
                    container_path: "/ro1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/ro2".into(),
                    container_path: "/ro2".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev1".into(),
                    container_path: "/dev1".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let new = NvidiaState {
            driver_version: "2.0".to_string(),
            binds: vec![
                PassthroughBind {
                    host_path: "/ro1".into(),
                    container_path: "/ro1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev1".into(),
                    container_path: "/dev1".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list, vec!["/ro2".to_string()]);
    }

    #[test]
    fn test_calculate_death_list_no_change() {
        let state = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        assert!(calculate_death_list(&state, &state).is_empty());
    }

    #[test]
    fn test_calculate_death_list_completely_new() {
        let old = NvidiaState::default();
        let new = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        assert!(calculate_death_list(&old, &new).is_empty());
    }

    #[test]
    fn test_calculate_death_list_everything_removed() {
        let old = NvidiaState {
            driver_version: "1.0".to_string(),
            binds: vec![PassthroughBind {
                host_path: "/ro1".into(),
                container_path: "/ro1".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        let new = NvidiaState::default();
        let death_list = calculate_death_list(&old, &new);
        assert_eq!(death_list, vec!["/ro1".to_string()]);
    }

    #[test]
    fn removed_binds_preserve_full_mount_semantics() {
        let old = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/old/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let new = NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/new/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            calculate_removed_binds(&old, &new),
            vec![PassthroughBind {
                host_path: "/old/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }]
        );
    }

    #[test]
    fn test_nvidia_state_serde_roundtrip() {
        let state = NvidiaState {
            driver_version: "550.1".to_string(),
            binds: vec![
                PassthroughBind {
                    host_path: "/usr/lib/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/nvidia0".into(),
                    readonly: false,
                },
            ],
            ..Default::default()
        };
        let serialized = serde_json::to_string(&state).unwrap();
        let deserialized: NvidiaState = serde_json::from_str(&serialized).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn test_nvidia_state_serde_empty_state() {
        let state = NvidiaState::default();
        let json = serde_json::to_string(&state).unwrap();
        let back: NvidiaState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.driver_version, "");
        assert!(back.binds.is_empty());
        assert!(back.readonly_binds.is_empty());
        assert!(back.device_binds.is_empty());
    }

    #[test]
    fn test_nvidia_state_serde_legacy_migration() {
        // Simulate an old-format state file (no "binds" key)
        let json = r#"{
            "driver_version": "550.1",
            "readonly_binds": ["/usr/lib/libcuda.so"],
            "device_binds": ["/dev/nvidia0"],
            "classified_entries": [{
                "host_path": "/host/gsp.bin",
                "default_container_path": "/lib/firmware/nvidia/gsp.bin",
                "category": "Firmware"
            }],
            "symlinks": [],
            "ldcache_folders": [],
            "env_vars": []
        }"#;
        let mut state: NvidiaState = serde_json::from_str(json).unwrap();
        assert!(state.binds.is_empty());
        state.migrate_from_legacy();
        assert_eq!(state.binds.len(), 3);
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/dev/nvidia0" && !b.readonly));
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/usr/lib/libcuda.so" && b.readonly));
        assert!(state
            .binds
            .iter()
            .any(|b| b.host_path == "/host/gsp.bin" && b.readonly));
    }

    #[test]
    fn operation_deserialization_rejects_invalid_machine_name() {
        let json = r#"{
            "operation": "read",
            "params": {"machine": "../escape"}
        }"#;
        assert!(serde_json::from_str::<NvidiaStateOperation>(json).is_err());
    }

    #[test]
    fn state_validation_rejects_relative_bind_paths() {
        let state = NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "relative/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            ..Default::default()
        };
        assert!(validate_state(&state).is_err());
    }

    fn test_state_directory() -> (tempfile::TempDir, TrustedDirectory) {
        let temporary = tempfile::tempdir().unwrap();
        let root = TrustedStateRoot::for_test(temporary.path().join("lasper"));
        let states = root.directory(StateDirectory::States).unwrap();
        (temporary, states)
    }

    #[test]
    fn initial_state_apply_replaces_owned_and_preserves_unknown_state() {
        let (temporary, directory) = test_state_directory();
        let path = temporary.path().join("lasper/states/test.json");
        let state = NvidiaState {
            driver_version: "1".into(),
            ..Default::default()
        };

        assert_eq!(
            write_initial_state(&directory, "test.json", &state).unwrap(),
            ResourceApplyStatus::Created
        );
        assert_eq!(
            write_initial_state(&directory, "test.json", &state).unwrap(),
            ResourceApplyStatus::Unchanged
        );
        let different = NvidiaState {
            driver_version: "2".into(),
            ..Default::default()
        };
        assert_eq!(
            write_initial_state(&directory, "test.json", &different).unwrap(),
            ResourceApplyStatus::ReplacedOwned
        );
        assert_eq!(
            read_state(&directory, "test.json")
                .unwrap()
                .unwrap()
                .driver_version,
            "2"
        );

        let mut unmarked = different.clone();
        unmarked.driver_version = "legacy".into();
        std::fs::write(&path, serde_json::to_string(&unmarked).unwrap()).unwrap();
        assert_eq!(
            write_initial_state(&directory, "test.json", &state).unwrap(),
            ResourceApplyStatus::ConflictUnknownOwner
        );
        assert_eq!(
            read_state(&directory, "test.json")
                .unwrap()
                .unwrap()
                .driver_version,
            "legacy"
        );

        directory.remove_with_lock("test.json").unwrap();
        assert!(!path.exists());
        assert!(!temporary
            .path()
            .join("lasper/states/.test.json.lock")
            .exists());
    }

    #[test]
    fn state_write_read_round_trip_is_atomic_and_lock_is_reclaimed_on_remove() {
        let (temporary, directory) = test_state_directory();
        let path = temporary.path().join("lasper/states/test.json");
        let state = NvidiaState {
            driver_version: "555.0".into(),
            binds: vec![PassthroughBind {
                host_path: "/host/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            ..Default::default()
        };

        write_state(&directory, "test.json", &state).unwrap();
        let read = read_state(&directory, "test.json").unwrap().unwrap();

        assert_eq!(read.driver_version, "555.0");
        assert_eq!(read.binds, state.binds);
        assert!(temporary
            .path()
            .join("lasper/states/.test.json.lock")
            .exists());
        directory.remove_with_lock("test.json").unwrap();
        assert!(!path.exists());
        assert!(!temporary
            .path()
            .join("lasper/states/.test.json.lock")
            .exists());
    }

    #[test]
    fn remove_state_ignores_missing_file() {
        let (temporary, directory) = test_state_directory();
        let path = temporary.path().join("lasper/states/missing.json");
        let lock_path = temporary.path().join("lasper/states/.missing.json.lock");
        std::fs::write(&lock_path, "").unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        directory.remove_with_lock("missing.json").unwrap();

        assert!(!path.exists());
        assert!(!lock_path.exists());
    }

    #[test]
    fn owned_state_cleanup_requires_marker_and_safe_metadata() {
        let (temporary, directory) = test_state_directory();
        let state_path = temporary.path().join("lasper/states");
        let owned = state_path.join("state.json");
        let legacy = state_path.join("legacy.json");
        let unsafe_mode = state_path.join("unsafe.json");
        let symlink = state_path.join("symlink.json");
        let state = NvidiaState {
            driver_version: "555.0".into(),
            ..Default::default()
        };
        write_state(&directory, "state.json", &state).unwrap();
        std::fs::write(&legacy, serde_json::to_string(&state).unwrap()).unwrap();
        let mut marked = state.clone();
        marked.ownership_marker = Some(NVIDIA_STATE_MARKER.into());
        std::fs::write(&unsafe_mode, serde_json::to_string(&marked).unwrap()).unwrap();
        std::fs::set_permissions(&unsafe_mode, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::os::unix::fs::symlink(&owned, &symlink).unwrap();

        assert_eq!(
            remove_owned_state(&directory, "state.json").unwrap(),
            ArtifactOwnership::ProvenOwned
        );
        assert!(!owned.exists());
        assert_eq!(
            remove_owned_state(&directory, "legacy.json").unwrap(),
            ArtifactOwnership::AmbiguousLegacy
        );
        assert_eq!(
            remove_owned_state(&directory, "unsafe.json").unwrap(),
            ArtifactOwnership::AmbiguousLegacy
        );
        assert_eq!(
            remove_owned_state(&directory, "symlink.json").unwrap(),
            ArtifactOwnership::AmbiguousLegacy
        );
        assert!(legacy.exists());
        assert!(unsafe_mode.exists());
        assert!(symlink.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            remove_owned_state(&directory, "missing.json").unwrap(),
            ArtifactOwnership::NotPresent
        );
    }
}
