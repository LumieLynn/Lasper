use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use crate::nspawn::sys::io::AsyncLockedWriter;
use ini::Ini;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_OVERRIDE_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_DEVICE_ALLOW_ENTRIES: usize = 4096;

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
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl SystemdUnitStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub async fn write_override(
        &self,
        name: &str,
        device_binds: &[String],
        nvidia_gpu: bool,
        graphics_acceleration: bool,
        wayland_socket: bool,
    ) -> Result<()> {
        if device_binds.is_empty() && !nvidia_gpu && !wayland_socket && !graphics_acceleration {
            return Ok(());
        }

        self.execute(SystemdUnitOperation::WriteOverride(WriteServiceOverride {
            machine: parse_machine_name(name)?,
            spec: ServiceOverrideSpec {
                device_binds: device_binds.to_vec(),
                nvidia_gpu,
                graphics_acceleration,
                wayland_socket,
            },
        }))
        .await?;
        Ok(())
    }

    pub async fn clone_override(&self, source: &str, destination: &str) -> Result<()> {
        self.execute(SystemdUnitOperation::CloneOverride(CloneServiceOverride {
            source: parse_machine_name(source)?,
            destination: parse_machine_name(destination)?,
        }))
        .await?;
        Ok(())
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

    pub async fn remove_overrides(&self, name: &str) -> Result<()> {
        self.execute(SystemdUnitOperation::RemoveOverrides(
            RemoveServiceOverrides {
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

    async fn execute(&self, operation: SystemdUnitOperation) -> Result<SystemdUnitResult> {
        if let Some(daemon) = &self.daemon {
            daemon
                .systemd_unit(operation)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            execute_systemd_unit_operation(operation).await
        }
    }
}

impl std::fmt::Debug for SystemdUnitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemdUnitStore")
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum SystemdUnitOperation {
    Read(ReadServiceOverrides),
    WriteOverride(WriteServiceOverride),
    CloneOverride(CloneServiceOverride),
    WriteNvidiaDeviceAllow(WriteNvidiaDeviceAllow),
    RemoveOverrides(RemoveServiceOverrides),
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
pub(crate) struct ServiceOverrideSpec {
    device_binds: Vec<String>,
    nvidia_gpu: bool,
    graphics_acceleration: bool,
    wayland_socket: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdUnitResult {
    #[serde(default)]
    drop_ins: Vec<SystemdDropIn>,
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
            return Ok(SystemdUnitResult { drop_ins });
        }
        SystemdUnitOperation::WriteOverride(request) => {
            validate_override_spec(&request.spec)?;
            let content = systemd_override_content(
                &request.spec.device_binds,
                request.spec.nvidia_gpu,
                request.spec.graphics_acceleration,
                request.spec.wayland_socket,
            );
            validate_content_size(&content)?;
            write_override_at(&service_override_path(&request.machine), &content).await?;
        }
        SystemdUnitOperation::CloneOverride(request) => {
            let source = service_override_path(&request.source);
            if let Some(content) = read_optional(&source).await? {
                validate_content_size(&content)?;
                write_override_at(&service_override_path(&request.destination), &content).await?;
            }
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
        SystemdUnitOperation::RemoveOverrides(request) => {
            remove_overrides_at(&service_override_dir(&request.machine)).await?;
        }
    }
    Ok(SystemdUnitResult::default())
}

/// Generate the content for a systemd service override.
pub fn systemd_override_content(
    device_binds: &[String],
    _nvidia_gpu: bool,
    _graphics_acceleration: bool,
    wayland_socket: bool,
) -> String {
    let mut conf = Ini::new();
    conf.with_section(Some("Service")).set("__placeholder", "");
    let s = conf.section_mut(Some("Service")).unwrap();
    s.remove("__placeholder");

    // if nvidia_gpu || wayland_socket {
    //     s.insert("Delegate", "yes");
    // }
    // Note: Delegate=yes is no longer used for GPU/Wayland passthrough to maintain
    // the Principle of Least Privilege and avoid cgroup management power-leaks.

    for dev in device_binds {
        s.append("DeviceAllow", format!("{} rw", dev));
    }
    if wayland_socket {
        s.append("DeviceAllow", "/dev/dri rw");
    }
    // Note: Individual device allows (/dev/dri, /dev/mali, etc.) are now
    // dynamically discovered and passed via device_binds.

    let mut buffer = Vec::new();
    conf.write_to(&mut buffer).unwrap_or_default();
    String::from_utf8_lossy(&buffer).into_owned()
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
    service_override_dir(machine).join("override.conf")
}

fn transient_service_override_dir(machine: &MachineName) -> PathBuf {
    PathBuf::from(format!(
        "/run/systemd/system/systemd-nspawn@{}.service.d",
        machine.as_str()
    ))
}

fn persistent_nvidia_override_path(machine: &MachineName) -> PathBuf {
    service_override_dir(machine).join("10-lasper-nvidia.conf")
}

fn transient_nvidia_override_path(machine: &MachineName) -> PathBuf {
    transient_service_override_dir(machine).join("10-lasper-nvidia.conf")
}

fn validate_override_spec(spec: &ServiceOverrideSpec) -> Result<()> {
    if spec.device_binds.len() > MAX_DEVICE_ALLOW_ENTRIES {
        return Err(NspawnError::Validation(
            "Too many DeviceAllow entries".into(),
        ));
    }

    for bind in &spec.device_binds {
        validate_device_allow(bind)?;
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
        validate_device_allow(path)?;
    }
    Ok(())
}

fn validate_device_allow(value: &str) -> Result<()> {
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

fn nvidia_device_allow_content(device_paths: &[String]) -> String {
    let mut content = String::from("[Service]\n");
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
        if !file_type.is_file() {
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
    AsyncLockedWriter::write_atomic(path, content).await
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

async fn remove_overrides_at(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_systemd_override_content_devices() {
        let binds = vec!["/dev/nvidia0".to_string(), "/dev/nvidiactl".to_string()];
        let content = systemd_override_content(&binds, false, false, false);
        assert!(content.contains("[Service]"));
        assert!(content.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(content.contains("DeviceAllow=/dev/nvidiactl rw"));
    }

    #[test]
    fn test_systemd_override_content_wayland() {
        let content = systemd_override_content(&[], false, false, true);
        assert!(content.contains("DeviceAllow=/dev/dri rw"));
    }

    #[test]
    fn test_systemd_override_content_empty_devices_no_wayland() {
        let content = systemd_override_content(&[], false, false, false);
        assert!(content.contains("[Service]"));
        assert!(!content.contains("DeviceAllow"));
    }

    #[test]
    fn test_systemd_override_content_combined() {
        let binds = vec!["/dev/nvidia0".to_string()];
        let content = systemd_override_content(&binds, true, true, true);
        assert!(content.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(content.contains("DeviceAllow=/dev/dri rw"));
        // nvidia_gpu and graphics_acceleration params are currently unused/commented out
        // They should NOT produce any additional output
        assert!(!content.contains("Delegate"));
    }

    #[test]
    fn test_systemd_override_content_is_valid_ini() {
        let binds = vec!["/dev/nvidia0".to_string()];
        let content = systemd_override_content(&binds, false, false, true);
        // Should be parseable as valid INI
        assert!(Ini::load_from_str(&content).is_ok());
    }

    #[test]
    fn operation_deserialization_rejects_invalid_machine_name() {
        let json = r#"{
            "operation": "remove_overrides",
            "params": {"machine": "../escape"}
        }"#;
        assert!(serde_json::from_str::<SystemdUnitOperation>(json).is_err());
    }

    #[test]
    fn override_spec_rejects_relative_device_allow_path() {
        let spec = ServiceOverrideSpec {
            device_binds: vec!["dev/nvidia0".into()],
            nvidia_gpu: false,
            graphics_acceleration: false,
            wayland_socket: false,
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
    }

    #[tokio::test]
    async fn write_override_is_atomic_without_persistent_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("override.conf");
        let content = systemd_override_content(&["/dev/nvidia0".into()], false, false, true);

        write_override_at(&path, &content).await.unwrap();

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("DeviceAllow=/dev/nvidia0 rw"));
        assert!(written.contains("DeviceAllow=/dev/dri rw"));
        assert!(!crate::nspawn::sys::io::lock_path_for(&path).exists());
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
    async fn remove_overrides_deletes_directory() {
        let directory = tempfile::tempdir().unwrap();
        let override_dir = directory.path().join("systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&override_dir).await.unwrap();
        tokio::fs::write(override_dir.join("override.conf"), "[Service]\n")
            .await
            .unwrap();

        remove_overrides_at(&override_dir).await.unwrap();

        assert!(!override_dir.exists());
    }

    #[tokio::test]
    async fn read_drop_ins_returns_regular_files_and_skips_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let override_dir = directory.path().join("systemd-nspawn@test.service.d");
        tokio::fs::create_dir_all(&override_dir).await.unwrap();
        tokio::fs::write(override_dir.join("override.conf"), "[Service]\n")
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
