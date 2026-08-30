use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::filesystem::AsyncLockedWriter;
use crate::application::image_lifecycle::ArtifactOwnership;
use crate::domain::machine::MachineName;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ApplyStatus;
use ini::Ini;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_OVERRIDE_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_DEVICE_ALLOW_ENTRIES: usize = 4096;
const LASPER_OVERRIDE_FILE: &str = "90-lasper.conf";
const LASPER_NVIDIA_OVERRIDE_FILE: &str = "90-lasper-nvidia.conf";
const LEGACY_OVERRIDE_FILE: &str = "override.conf";
const LEGACY_NVIDIA_OVERRIDE_FILE: &str = "10-lasper-nvidia.conf";
const LASPER_OVERRIDE_MARKER: &str = "# Managed by Lasper: systemd unit override v1";
const ALL_DRM_DEVICE_ALLOW: &str = "char-drm";

/// Read-only view of the host unit drop-ins associated with a machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemdUnitInspection {
    pub unit: String,
    pub drop_ins: Vec<SystemdDropIn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemdDropIn {
    pub path: String,
    pub content: String,
}

/// Typed access to Lasper-managed `systemd-nspawn@.service` overrides.
#[derive(Clone)]
pub struct SystemdUnitStore {
    executor: Arc<dyn SystemdUnitExecutor>,
}

impl SystemdUnitStore {
    pub(crate) fn direct() -> Self {
        Self {
            executor: Arc::new(DirectSystemdUnitExecutor),
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            executor: Arc::new(ElevatedSystemdUnitExecutor { daemon }),
        }
    }

    pub async fn write_override(
        &self,
        name: &str,
        device_binds: &[String],
        gpu_passthrough_all: bool,
    ) -> Result<ApplyStatus> {
        if device_binds.is_empty() && !gpu_passthrough_all {
            return Ok(ApplyStatus::Unchanged);
        }

        let result = self
            .execute(SystemdUnitOperation::WriteOverride(WriteServiceOverride {
                machine: parse_machine_name(name)?,
                spec: ServiceOverrideSpec {
                    device_binds: device_binds.to_vec(),
                    gpu_passthrough_all,
                },
            }))
            .await?;
        result.apply.ok_or_else(|| {
            NspawnError::Runtime("systemd override write returned no apply status".into())
        })
    }

    pub async fn clone_override(&self, source: &str, destination: &str) -> Result<ApplyStatus> {
        let result = self
            .execute(SystemdUnitOperation::CloneOverride(CloneServiceOverride {
                source: parse_machine_name(source)?,
                destination: parse_machine_name(destination)?,
            }))
            .await?;
        result.apply.ok_or_else(|| {
            NspawnError::Runtime("systemd override clone returned no apply status".into())
        })
    }

    pub async fn write_nvidia_device_allow(
        &self,
        name: &str,
        device_paths: &[String],
    ) -> Result<()> {
        self.execute(SystemdUnitOperation::WriteNvidiaDeviceAllow(
            WriteNvidiaDeviceAllow {
                machine: parse_machine_name(name)?,
                device_paths: device_paths.to_vec(),
            },
        ))
        .await?;
        Ok(())
    }

    /// Remove only current Lasper drop-ins whose marker and file safety checks
    /// prove ownership. Legacy unmarked names are deliberately preserved.
    pub async fn remove_owned_overrides(&self, name: &str) -> Result<Vec<ArtifactOwnership>> {
        let result = self
            .execute(SystemdUnitOperation::RemoveOwnedOverrides(
                RemoveServiceOverrides {
                    machine: parse_machine_name(name)?,
                },
            ))
            .await?;
        result.ownership.ok_or_else(|| {
            NspawnError::Runtime("owned override cleanup returned no ownership evidence".into())
        })
    }

    pub async fn remove_service_override(&self, name: &str) -> Result<()> {
        self.execute(SystemdUnitOperation::RemoveOverride(
            RemoveServiceOverride {
                machine: parse_machine_name(name)?,
            },
        ))
        .await?;
        Ok(())
    }

    pub async fn read(&self, name: &str) -> Result<SystemdUnitInspection> {
        let machine = parse_machine_name(name)?;
        let unit = machine.systemd_nspawn_unit();
        let result = self
            .execute(SystemdUnitOperation::Read(ReadServiceOverrides { machine }))
            .await?;
        Ok(SystemdUnitInspection {
            unit,
            drop_ins: result.drop_ins,
        })
    }

    pub(crate) async fn probe_owned_overrides(&self, name: &str) -> Result<Vec<ArtifactOwnership>> {
        let result = self
            .execute(SystemdUnitOperation::ProbeOwnedOverrides(
                ReadServiceOverrides {
                    machine: parse_machine_name(name)?,
                },
            ))
            .await?;
        result.ownership.ok_or_else(|| {
            NspawnError::Runtime("override probe returned no ownership evidence".into())
        })
    }

    async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult> {
        self.executor.execute(operation).await
    }
}

impl std::fmt::Debug for SystemdUnitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemdUnitStore")
            .field("route", &self.executor.route())
            .finish()
    }
}

#[async_trait::async_trait]
trait SystemdUnitExecutor: Send + Sync + 'static {
    fn route(&self) -> &'static str;

    async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult>;
}

struct DirectSystemdUnitExecutor;

#[async_trait::async_trait]
impl SystemdUnitExecutor for DirectSystemdUnitExecutor {
    fn route(&self) -> &'static str {
        "direct"
    }

    async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult> {
        execute_systemd_unit_operation(operation).await
    }
}

struct ElevatedSystemdUnitExecutor {
    daemon: Arc<ElevatedDaemon>,
}

#[async_trait::async_trait]
impl SystemdUnitExecutor for ElevatedSystemdUnitExecutor {
    fn route(&self) -> &'static str {
        "elevated_rpc"
    }

    async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult> {
        self.daemon
            .systemd_unit(operation)
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum SystemdUnitOperation {
    Read(ReadServiceOverrides),
    ProbeOwnedOverrides(ReadServiceOverrides),
    WriteOverride(WriteServiceOverride),
    CloneOverride(CloneServiceOverride),
    WriteNvidiaDeviceAllow(WriteNvidiaDeviceAllow),
    RemoveOverride(RemoveServiceOverride),
    RemoveOwnedOverrides(RemoveServiceOverrides),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadServiceOverrides {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteServiceOverride {
    machine: MachineName,
    spec: ServiceOverrideSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloneServiceOverride {
    source: MachineName,
    destination: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteNvidiaDeviceAllow {
    machine: MachineName,
    device_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveServiceOverrides {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveServiceOverride {
    machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceOverrideSpec {
    device_binds: Vec<String>,
    #[serde(default)]
    gpu_passthrough_all: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdUnitResult {
    #[serde(default)]
    drop_ins: Vec<SystemdDropIn>,
    #[serde(default)]
    apply: Option<ApplyStatus>,
    #[serde(default)]
    ownership: Option<Vec<ArtifactOwnership>>,
}

pub(crate) async fn execute_systemd_unit_operation(
    operation: SystemdUnitOperation,
) -> Result<SystemdUnitResult> {
    match operation {
        SystemdUnitOperation::Read(request) => {
            let mut drop_ins = Vec::new();
            drop_ins.extend(read_drop_ins(&service_override_dir(&request.machine)).await?);
            drop_ins
                .extend(read_drop_ins(&transient_service_override_dir(&request.machine)).await?);
            drop_ins.sort_by(|left, right| left.path.cmp(&right.path));
            return Ok(SystemdUnitResult {
                drop_ins,
                ..Default::default()
            });
        }
        SystemdUnitOperation::ProbeOwnedOverrides(request) => {
            return Ok(SystemdUnitResult {
                ownership: Some(
                    probe_lasper_overrides_at_with_uid(
                        &service_override_dir(&request.machine),
                        &transient_service_override_dir(&request.machine),
                        0,
                    )
                    .await?,
                ),
                ..Default::default()
            });
        }
        SystemdUnitOperation::WriteOverride(request) => {
            validate_override_spec(&request.spec)?;
            let content = systemd_override_content(
                &request.spec.device_binds,
                request.spec.gpu_passthrough_all,
            );
            validate_content_size(&content)?;
            let apply =
                apply_new_override_at(&service_override_path(&request.machine), content).await?;
            return Ok(SystemdUnitResult {
                apply: Some(apply),
                ..Default::default()
            });
        }
        SystemdUnitOperation::CloneOverride(request) => {
            let source = service_override_path(&request.source);
            let apply = if let Some(content) = read_optional(&source).await? {
                validate_content_size(&content)?;
                apply_new_override_at(&service_override_path(&request.destination), content).await?
            } else {
                ApplyStatus::Unchanged
            };
            return Ok(SystemdUnitResult {
                apply: Some(apply),
                ..Default::default()
            });
        }
        SystemdUnitOperation::WriteNvidiaDeviceAllow(request) => {
            validate_device_allow_paths(&request.device_paths)?;
            let content = nvidia_device_allow_content(&request.device_paths);
            validate_content_size(&content)?;
            write_nvidia_device_allow_at(
                &persistent_nvidia_override_path(&request.machine),
                &transient_nvidia_override_path(&request.machine),
                &content,
            )
            .await?;
        }
        SystemdUnitOperation::RemoveOverride(request) => {
            remove_service_override_at(&service_override_dir(&request.machine)).await?;
        }
        SystemdUnitOperation::RemoveOwnedOverrides(request) => {
            let ownership = remove_owned_lasper_overrides_at(
                &service_override_dir(&request.machine),
                &transient_service_override_dir(&request.machine),
            )
            .await?;
            return Ok(SystemdUnitResult {
                ownership: Some(ownership),
                ..Default::default()
            });
        }
    }
    Ok(SystemdUnitResult::default())
}

/// Generate the content for a systemd service override.
pub fn systemd_override_content(device_binds: &[String], gpu_passthrough_all: bool) -> String {
    let passthrough_all_drm = gpu_passthrough_all
        || device_binds
            .iter()
            .any(|path| path == crate::nspawn::models::ALL_DRM_DEVICES_PATH);
    let mut conf = Ini::new();
    conf.with_section(Some("Service")).set("__placeholder", "");
    let s = conf.section_mut(Some("Service")).unwrap();
    s.remove("__placeholder");

    for dev in device_binds {
        if dev == crate::nspawn::models::ALL_DRM_DEVICES_PATH {
            continue;
        }
        s.append("DeviceAllow", format!("{} rw", dev));
    }
    if passthrough_all_drm {
        s.append("DeviceAllow", format!("{ALL_DRM_DEVICE_ALLOW} rw"));
    }

    let mut buffer = Vec::new();
    conf.write_to(&mut buffer).unwrap_or_default();
    format!(
        "{LASPER_OVERRIDE_MARKER}\n{}",
        String::from_utf8_lossy(&buffer)
    )
}

/// Write a systemd service override to allow devices via cgroups.
fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn service_override_dir(machine: &MachineName) -> PathBuf {
    PathBuf::from(format!(
        "/etc/systemd/system/systemd-nspawn@{}.service.d",
        machine.as_str()
    ))
}

fn service_override_path(machine: &MachineName) -> PathBuf {
    service_override_dir(machine).join(LASPER_OVERRIDE_FILE)
}

fn transient_service_override_dir(machine: &MachineName) -> PathBuf {
    PathBuf::from(format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d",
        machine.as_str()
    ))
}

fn persistent_nvidia_override_path(machine: &MachineName) -> PathBuf {
    service_override_dir(machine).join(LASPER_NVIDIA_OVERRIDE_FILE)
}

fn transient_nvidia_override_path(machine: &MachineName) -> PathBuf {
    transient_service_override_dir(machine).join(LASPER_NVIDIA_OVERRIDE_FILE)
}

fn validate_override_spec(spec: &ServiceOverrideSpec) -> Result<()> {
    if spec.device_binds.len() > MAX_DEVICE_ALLOW_ENTRIES {
        return Err(NspawnError::Validation(
            "Too many DeviceAllow entries".into(),
        ));
    }

    for bind in &spec.device_binds {
        validate_device_allow_path(bind)?;
    }
    Ok(())
}

fn validate_device_allow_paths(device_paths: &[String]) -> Result<()> {
    if device_paths.len() > MAX_DEVICE_ALLOW_ENTRIES {
        return Err(NspawnError::Validation(
            "Too many DeviceAllow entries".into(),
        ));
    }

    for path in device_paths {
        validate_device_allow_path(path)?;
    }
    Ok(())
}

fn validate_device_allow_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || !Path::new(value).is_absolute()
    {
        return Err(NspawnError::Validation(format!(
            "Invalid DeviceAllow path: {value:?}"
        )));
    }
    Ok(())
}

fn validate_device_allow_spec(value: &str) -> Result<()> {
    if value == ALL_DRM_DEVICE_ALLOW {
        Ok(())
    } else {
        validate_device_allow_path(value)
    }
}

fn nvidia_device_allow_content(device_paths: &[String]) -> String {
    let mut content = format!("{LASPER_OVERRIDE_MARKER}\n[Service]\n");
    for path in device_paths {
        content.push_str(&format!("DeviceAllow={} rw\n", path));
    }
    content
}

fn validate_content_size(content: &str) -> Result<()> {
    if content.len() > MAX_OVERRIDE_CONTENT_BYTES {
        return Err(NspawnError::Validation(format!(
            "systemd override content exceeds {} bytes",
            MAX_OVERRIDE_CONTENT_BYTES
        )));
    }
    Ok(())
}

async fn read_optional(path: &Path) -> Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn read_drop_ins(dir: &Path) -> Result<Vec<SystemdDropIn>> {
    let mut reader = match tokio::fs::read_dir(dir).await {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(NspawnError::Io(dir.to_path_buf(), error)),
    };
    let mut drop_ins = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|error| NspawnError::Io(dir.to_path_buf(), error))?
    {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|error| NspawnError::Io(path.clone(), error))?;
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("conf")
        {
            continue;
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| NspawnError::Io(path.clone(), error))?;
        validate_content_size(&content)?;
        drop_ins.push(SystemdDropIn {
            path: path.display().to_string(),
            content,
        });
    }
    Ok(drop_ins)
}

async fn write_override_at(path: &Path, content: &str) -> Result<()> {
    AsyncLockedWriter::write_atomic_with_mode(path, content, Some(0o644)).await
}

async fn apply_new_override_at(path: &Path, content: String) -> Result<ApplyStatus> {
    apply_new_override_at_with_uid(path, content, 0).await
}

async fn apply_new_override_at_with_uid(
    path: &Path,
    content: String,
    expected_uid: u32,
) -> Result<ApplyStatus> {
    let ownership = probe_owned_override_at(path, expected_uid).await?;
    if ownership == ArtifactOwnership::AmbiguousLegacy {
        return Ok(ApplyStatus::ConflictUnknownOwner);
    }
    AsyncLockedWriter::apply_locked_with_mode(path, 0o644, move |existing| {
        Ok(match existing {
            None => (Some(content), ApplyStatus::Created),
            Some(existing) if existing == content => (None, ApplyStatus::Unchanged),
            Some(existing)
                if ownership == ArtifactOwnership::ProvenOwned
                    && is_owned_override_content(&existing) =>
            {
                (Some(content), ApplyStatus::ReplacedOwned)
            }
            Some(_) => (None, ApplyStatus::ConflictUnknownOwner),
        })
    })
    .await
}

async fn write_nvidia_device_allow_at(
    persistent_path: &Path,
    transient_path: &Path,
    content: &str,
) -> Result<()> {
    write_override_at(persistent_path, content).await?;
    if let Err(error) = remove_optional_file(transient_path).await {
        log::warn!(
            "Failed to remove transient NVIDIA service override {}: {}",
            transient_path.display(),
            error
        );
    }
    Ok(())
}

async fn remove_optional_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_empty_dir(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_service_override_at(persistent_dir: &Path) -> Result<()> {
    let override_path = persistent_dir.join(LASPER_OVERRIDE_FILE);
    remove_optional_file(&override_path).await?;
    remove_optional_file(&crate::adapters::filesystem::lock_path_for(&override_path)).await?;
    remove_empty_dir(persistent_dir).await
}

async fn remove_owned_lasper_overrides_at(
    persistent_dir: &Path,
    transient_dir: &Path,
) -> Result<Vec<ArtifactOwnership>> {
    remove_owned_lasper_overrides_at_with_uid(persistent_dir, transient_dir, 0).await
}

async fn remove_owned_lasper_overrides_at_with_uid(
    persistent_dir: &Path,
    transient_dir: &Path,
    expected_uid: u32,
) -> Result<Vec<ArtifactOwnership>> {
    let owned_paths = [
        persistent_dir.join(LASPER_OVERRIDE_FILE),
        persistent_dir.join(LASPER_NVIDIA_OVERRIDE_FILE),
        transient_dir.join(LASPER_NVIDIA_OVERRIDE_FILE),
    ];
    let legacy_paths = [
        persistent_dir.join(LEGACY_OVERRIDE_FILE),
        persistent_dir.join(LEGACY_NVIDIA_OVERRIDE_FILE),
        transient_dir.join(LEGACY_NVIDIA_OVERRIDE_FILE),
    ];
    let mut ownership = Vec::with_capacity(owned_paths.len() + legacy_paths.len());
    for path in owned_paths {
        let evidence = probe_owned_override_at(&path, expected_uid).await?;
        match evidence {
            ArtifactOwnership::ProvenOwned => {
                remove_optional_file(&path).await?;
                remove_optional_file(&crate::adapters::filesystem::lock_path_for(&path)).await?;
            }
            ArtifactOwnership::NotPresent => {
                let lock_path = crate::adapters::filesystem::lock_path_for(&path);
                AsyncLockedWriter::remove_lock_if_target_absent(&path, &lock_path).await?;
            }
            ArtifactOwnership::AmbiguousLegacy => {}
        }
        ownership.push(evidence);
    }
    for path in legacy_paths {
        ownership.push(probe_legacy_override_at(&path).await?);
    }
    for directory in [persistent_dir, transient_dir] {
        remove_empty_dir(directory).await?;
    }
    Ok(ownership)
}

async fn probe_lasper_overrides_at_with_uid(
    persistent_dir: &Path,
    transient_dir: &Path,
    expected_uid: u32,
) -> Result<Vec<ArtifactOwnership>> {
    let owned_paths = [
        persistent_dir.join(LASPER_OVERRIDE_FILE),
        persistent_dir.join(LASPER_NVIDIA_OVERRIDE_FILE),
        transient_dir.join(LASPER_NVIDIA_OVERRIDE_FILE),
    ];
    let legacy_paths = [
        persistent_dir.join(LEGACY_OVERRIDE_FILE),
        persistent_dir.join(LEGACY_NVIDIA_OVERRIDE_FILE),
        transient_dir.join(LEGACY_NVIDIA_OVERRIDE_FILE),
    ];
    let mut ownership = Vec::with_capacity(owned_paths.len() + legacy_paths.len());
    for path in owned_paths {
        ownership.push(probe_owned_override_at(&path, expected_uid).await?);
    }
    for path in legacy_paths {
        ownership.push(probe_legacy_override_at(&path).await?);
    }
    Ok(ownership)
}

async fn probe_owned_override_at(path: &Path, expected_uid: u32) -> Result<ArtifactOwnership> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ArtifactOwnership::NotPresent)
        }
        Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
    };
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_OVERRIDE_CONTENT_BYTES as u64
        || metadata.uid() != expected_uid
        || (metadata.permissions().mode() & 0o7777) != 0o644
    {
        return Ok(ArtifactOwnership::AmbiguousLegacy);
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    Ok(if is_owned_override_content(&content) {
        ArtifactOwnership::ProvenOwned
    } else {
        ArtifactOwnership::AmbiguousLegacy
    })
}

async fn probe_legacy_override_at(path: &Path) -> Result<ArtifactOwnership> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(ArtifactOwnership::AmbiguousLegacy),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ArtifactOwnership::NotPresent)
        }
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

fn is_owned_override_content(content: &str) -> bool {
    if !content.starts_with(&format!("{LASPER_OVERRIDE_MARKER}\n")) {
        return false;
    }
    let Ok(config) = Ini::load_from_str(content) else {
        return false;
    };
    let mut saw_service = false;
    for (section, properties) in &config {
        match section {
            None if properties.is_empty() => {}
            Some("Service") => {
                saw_service = true;
                if properties.iter().any(|(key, value)| {
                    key != "DeviceAllow"
                        || match value.strip_suffix(" rw") {
                            Some(path) => validate_device_allow_spec(path).is_err(),
                            None => true,
                        }
                }) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    saw_service
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubSystemdUnitExecutor;

    #[async_trait::async_trait]
    impl SystemdUnitExecutor for StubSystemdUnitExecutor {
        fn route(&self) -> &'static str {
            "test"
        }

        async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult> {
            assert!(matches!(operation, SystemdUnitOperation::Read(_)));
            Ok(SystemdUnitResult {
                drop_ins: vec![SystemdDropIn {
                    path: "/etc/systemd/system/systemd-nspawn@test.service.d/90-lasper.conf".into(),
                    content: "[Service]\nDeviceAllow=char-drm rw\n".into(),
                }],
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn store_delegates_to_its_fixed_executor() {
        let store = SystemdUnitStore {
            executor: Arc::new(StubSystemdUnitExecutor),
        };

        let inspection = store.read("test").await.unwrap();

        assert_eq!(inspection.unit, "systemd-nspawn@test.service");
        assert_eq!(inspection.drop_ins.len(), 1);
        assert!(format!("{store:?}").contains("test"));
    }

    #[test]
    fn test_systemd_override_content_devices() {
        let binds = vec!["/dev/nvidia0".to_string(), "/dev/nvidiactl".to_string()];
        let content = systemd_override_content(&binds, false);
        assert!(content.contains("[Service]"));
        assert!(content.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(content.contains("DeviceAllow=/dev/nvidiactl rw"));
    }

    #[test]
    fn test_systemd_override_content_all_drm() {
        let content = systemd_override_content(&[], true);
        assert!(content.contains("DeviceAllow=char-drm rw"));
        assert!(!content.contains("DeviceAllow=/dev/dri rw"));
        assert!(is_owned_override_content(&content));

        let legacy_explicit_path =
            systemd_override_content(&[crate::nspawn::models::ALL_DRM_DEVICES_PATH.into()], false);
        assert!(legacy_explicit_path.contains("DeviceAllow=char-drm rw"));
        assert!(!legacy_explicit_path.contains("DeviceAllow=/dev/dri rw"));
    }

    #[test]
    fn test_systemd_override_content_empty_devices() {
        let content = systemd_override_content(&[], false);
        assert!(content.contains("[Service]"));
        assert!(!content.contains("DeviceAllow"));
    }

    #[test]
    fn test_systemd_override_content_combined() {
        let binds = vec!["/dev/nvidia0".to_string()];
        let content = systemd_override_content(&binds, true);
        assert!(content.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(content.contains("DeviceAllow=char-drm rw"));
    }

    #[test]
    fn test_systemd_override_content_is_valid_ini() {
        let binds = vec!["/dev/nvidia0".to_string()];
        let content = systemd_override_content(&binds, false);
        // Should be parseable as valid INI
        assert!(Ini::load_from_str(&content).is_ok());
    }

    #[test]
    fn operation_deserialization_rejects_invalid_machine_name() {
        let json = r#"{
            "operation": "remove_owned_overrides",
            "params": {"machine": "../escape"}
        }"#;
        assert!(serde_json::from_str::<SystemdUnitOperation>(json).is_err());
    }

    #[test]
    fn override_spec_rejects_relative_device_allow_path() {
        let spec = ServiceOverrideSpec {
            device_binds: vec!["dev/nvidia0".into()],
            gpu_passthrough_all: false,
        };
        assert!(validate_override_spec(&spec).is_err());
    }

    #[test]
    fn nvidia_device_allow_rejects_relative_device_path() {
        assert!(validate_device_allow_paths(&["dev/nvidia0".into()]).is_err());
    }

    #[test]
    fn nvidia_device_allow_content_uses_service_section() {
        let content = nvidia_device_allow_content(&["/dev/nvidia0".into()]);
        assert!(content.contains("[Service]"));
        assert!(content.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(Ini::load_from_str(&content).is_ok());
        assert!(is_owned_override_content(&content));
        assert!(!is_owned_override_content(&format!(
            "{LASPER_OVERRIDE_MARKER}\n[Service]\nExecStart=/bin/sh\n"
        )));
    }

    #[tokio::test]
    async fn write_override_is_atomic_without_persistent_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("override.conf");
        let content = systemd_override_content(&["/dev/nvidia0".into()], false);

        write_override_at(&path, &content).await.unwrap();

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(!written.contains("DeviceAllow=/dev/dri rw"));
        assert!(!crate::adapters::filesystem::lock_path_for(&path).exists());
    }

    #[tokio::test]
    async fn deployment_override_apply_replaces_owned_and_preserves_unknown_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("override.conf");
        let content = systemd_override_content(&["/dev/nvidia0".into()], false);

        assert_eq!(
            apply_new_override_at_with_uid(&path, content.clone(), uzers::get_current_uid())
                .await
                .unwrap(),
            ApplyStatus::Created
        );
        assert_eq!(
            apply_new_override_at_with_uid(&path, content.clone(), uzers::get_current_uid())
                .await
                .unwrap(),
            ApplyStatus::Unchanged
        );
        let replacement = systemd_override_content(&["/dev/dri/card0".into()], false);
        assert_eq!(
            apply_new_override_at_with_uid(&path, replacement.clone(), uzers::get_current_uid(),)
                .await
                .unwrap(),
            ApplyStatus::ReplacedOwned
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), replacement);

        tokio::fs::write(&path, "[Service]\nPrivateDevices=yes\n")
            .await
            .unwrap();
        assert_eq!(
            apply_new_override_at_with_uid(
                &path,
                systemd_override_content(&["/dev/nvidia1".into()], false),
                uzers::get_current_uid(),
            )
            .await
            .unwrap(),
            ApplyStatus::ConflictUnknownOwner
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "[Service]\nPrivateDevices=yes\n"
        );
    }

    #[tokio::test]
    async fn exact_override_rollback_preserves_other_drop_ins() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = directory.path().join("systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&persistent).await.unwrap();
        let override_path = persistent.join(LASPER_OVERRIDE_FILE);
        apply_new_override_at(&override_path, "[Service]\n".into())
            .await
            .unwrap();
        tokio::fs::write(
            persistent.join(LASPER_NVIDIA_OVERRIDE_FILE),
            "[Service]\nDeviceAllow=/dev/nvidia0 rw\n",
        )
        .await
        .unwrap();

        remove_service_override_at(&persistent).await.unwrap();

        assert!(!override_path.exists());
        assert!(persistent.join(LASPER_NVIDIA_OVERRIDE_FILE).exists());
        assert!(persistent.exists());
    }

    #[tokio::test]
    async fn write_nvidia_device_allow_removes_transient_override() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = directory.path().join("etc/10-lasper-nvidia.conf");
        let transient = directory.path().join("run/10-lasper-nvidia.conf");
        tokio::fs::create_dir_all(transient.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&transient, "[Service]\nDeviceAllow=/dev/old rw\n")
            .await
            .unwrap();
        let content = nvidia_device_allow_content(&["/dev/nvidia0".into()]);

        write_nvidia_device_allow_at(&persistent, &transient, &content)
            .await
            .unwrap();

        let written = tokio::fs::read_to_string(&persistent).await.unwrap();
        assert!(written.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(!transient.exists());
    }

    #[tokio::test]
    async fn owned_override_cleanup_requires_marker_and_safe_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = directory.path().join("etc/systemd-nspawn@test.service.d");
        let transient = directory.path().join("run/systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&persistent).await.unwrap();
        tokio::fs::create_dir_all(&transient).await.unwrap();

        let owned = persistent.join(LASPER_OVERRIDE_FILE);
        let legacy = persistent.join(LASPER_NVIDIA_OVERRIDE_FILE);
        let legacy_override = persistent.join(LEGACY_OVERRIDE_FILE);
        let unsafe_mode = transient.join(LASPER_NVIDIA_OVERRIDE_FILE);
        let owned_content = systemd_override_content(&[], true);
        assert!(
            is_owned_override_content(&owned_content),
            "{owned_content:?}"
        );
        tokio::fs::write(&owned, owned_content).await.unwrap();
        tokio::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();
        tokio::fs::write(&legacy, "[Service]\nDeviceAllow=/dev/nvidia0 rw\n")
            .await
            .unwrap();
        tokio::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();
        tokio::fs::write(&legacy_override, "[Service]\nDeviceAllow=/dev/dri rw\n")
            .await
            .unwrap();
        tokio::fs::write(
            &unsafe_mode,
            nvidia_device_allow_content(&["/dev/nvidia0".into()]),
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&unsafe_mode, std::fs::Permissions::from_mode(0o666))
            .await
            .unwrap();

        let ownership = remove_owned_lasper_overrides_at_with_uid(
            &persistent,
            &transient,
            uzers::get_current_uid(),
        )
        .await
        .unwrap();

        assert_eq!(
            ownership[..3],
            [
                ArtifactOwnership::ProvenOwned,
                ArtifactOwnership::AmbiguousLegacy,
                ArtifactOwnership::AmbiguousLegacy,
            ]
        );
        assert_eq!(ownership[3], ArtifactOwnership::AmbiguousLegacy);
        assert!(ownership[4..]
            .iter()
            .all(|evidence| *evidence == ArtifactOwnership::NotPresent));
        assert!(!owned.exists());
        assert!(legacy.exists());
        assert!(legacy_override.exists());
        assert!(unsafe_mode.exists());
        assert!(persistent.exists());
        assert!(transient.exists());
    }

    #[tokio::test]
    async fn recovery_override_probe_is_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = directory.path().join("etc/systemd-nspawn@test.service.d");
        let transient = directory.path().join("run/systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&persistent).await.unwrap();
        let owned = persistent.join(LASPER_OVERRIDE_FILE);
        tokio::fs::write(&owned, systemd_override_content(&[], true))
            .await
            .unwrap();
        tokio::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();

        let ownership =
            probe_lasper_overrides_at_with_uid(&persistent, &transient, uzers::get_current_uid())
                .await
                .unwrap();

        assert_eq!(ownership[0], ArtifactOwnership::ProvenOwned);
        assert!(ownership[1..]
            .iter()
            .all(|evidence| *evidence == ArtifactOwnership::NotPresent));
        assert!(owned.exists());
    }

    #[tokio::test]
    async fn owned_override_cleanup_removes_orphan_locks_and_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = directory.path().join("etc/systemd-nspawn@test.service.d");
        let transient = directory.path().join("run/systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&persistent).await.unwrap();
        tokio::fs::create_dir_all(&transient).await.unwrap();

        let paths = [
            persistent.join(LASPER_OVERRIDE_FILE),
            persistent.join(LASPER_NVIDIA_OVERRIDE_FILE),
            transient.join(LASPER_NVIDIA_OVERRIDE_FILE),
        ];
        for path in &paths {
            tokio::fs::write(crate::adapters::filesystem::lock_path_for(path), "")
                .await
                .unwrap();
        }

        let ownership = remove_owned_lasper_overrides_at_with_uid(
            &persistent,
            &transient,
            uzers::get_current_uid(),
        )
        .await
        .unwrap();

        assert!(ownership
            .iter()
            .all(|evidence| *evidence == ArtifactOwnership::NotPresent));
        assert!(paths
            .iter()
            .all(|path| !crate::adapters::filesystem::lock_path_for(path).exists()));
        assert!(!persistent.exists());
        assert!(!transient.exists());
    }

    #[tokio::test]
    async fn owned_override_probe_preserves_candidate_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.conf");
        let candidate = directory.path().join(LASPER_OVERRIDE_FILE);
        tokio::fs::write(
            &target,
            systemd_override_content(&["/dev/nvidia0".into()], false),
        )
        .await
        .unwrap();
        std::os::unix::fs::symlink(&target, &candidate).unwrap();

        assert_eq!(
            probe_owned_override_at(&candidate, uzers::get_current_uid())
                .await
                .unwrap(),
            ArtifactOwnership::AmbiguousLegacy
        );
        assert!(candidate
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    async fn read_drop_ins_returns_regular_files_and_skips_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let override_dir = directory.path().join("systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&override_dir).await.unwrap();
        tokio::fs::write(override_dir.join("override.conf"), "[Service]\n")
            .await
            .unwrap();
        tokio::fs::write(
            override_dir.join(".90-lasper.conf.lock"),
            "not a systemd drop-in",
        )
        .await
        .unwrap();
        tokio::fs::write(override_dir.join("README"), "not a systemd drop-in")
            .await
            .unwrap();
        tokio::fs::create_dir(override_dir.join("nested"))
            .await
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            override_dir.join("override.conf"),
            override_dir.join("link.conf"),
        )
        .unwrap();

        let result = read_drop_ins(&override_dir).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result[0].path.ends_with("override.conf"));
        assert_eq!(result[0].content, "[Service]\n");
    }
}
