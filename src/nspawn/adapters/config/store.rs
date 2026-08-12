use crate::nspawn::adapters::config::nspawn_file::{
    nspawn_config_content_from_spec_with_wayland_path, NspawnConfig,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerConfig, ImageName, MachineName, NspawnConfigSpec};
use crate::nspawn::platform::nvidia::NvidiaState;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use crate::nspawn::sys::io::AsyncLockedWriter;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_NSPAWN_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_NVIDIA_BINDS: usize = 16384;

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
    ) -> Result<()> {
        let spec = NspawnConfigSpec::try_from(config)?;
        self.execute(NspawnConfigOperation::Write(WriteNspawnConfig {
            spec,
            xdg_runtime: xdg_runtime.map(str::to_string),
            nvidia_state: nvidia_state.cloned(),
        }))
        .await?;
        Ok(())
    }

    pub async fn clone_config(&self, source: &str, destination: &str) -> Result<()> {
        self.execute(NspawnConfigOperation::Clone(CloneNspawnConfig {
            source: parse_machine_name(source)?,
            destination: parse_machine_name(destination)?,
        }))
        .await?;
        Ok(())
    }

    pub async fn update_gpu(
        &self,
        name: &str,
        state: &NvidiaState,
        death_list: &[String],
    ) -> Result<()> {
        self.execute(NspawnConfigOperation::UpdateGpu(UpdateNspawnGpu {
            machine: parse_machine_name(name)?,
            state: state.clone(),
            death_list: death_list.to_vec(),
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
    Clone(CloneNspawnConfig),
    UpdateGpu(UpdateNspawnGpu),
    Remove(RemoveNspawnConfig),
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
pub(crate) struct CloneNspawnConfig {
    source: MachineName,
    destination: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateNspawnGpu {
    machine: MachineName,
    state: NvidiaState,
    death_list: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NspawnConfigResult {
    content: Option<NspawnConfigInspection>,
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

pub(crate) async fn execute_nspawn_config_operation(
    operation: NspawnConfigOperation,
    invoking_uid: u32,
) -> Result<NspawnConfigResult> {
    match operation {
        NspawnConfigOperation::Read(request) => {
            let path = nspawn_path(&request.machine);
            Ok(NspawnConfigResult {
                content: read_config_at(&path).await?,
            })
        }
        NspawnConfigOperation::Inspect(request) => Ok(NspawnConfigResult {
            content: read_discovered_config(&request.image).await?,
        }),
        NspawnConfigOperation::Write(request) => {
            write_generated_at(
                &nspawn_path(&request.spec.machine),
                &request.spec,
                request.xdg_runtime.as_deref(),
                request.nvidia_state.as_ref(),
                invoking_uid,
            )
            .await?;
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::Clone(request) => {
            let source = nspawn_path(&request.source);
            if let Some(content) = read_optional(&source).await? {
                validate_content_size(&content)?;
                write_content(&nspawn_path(&request.destination), content).await?;
            }
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::UpdateGpu(request) => {
            update_gpu_at(
                &nspawn_path(&request.machine),
                request.machine,
                request.state,
                request.death_list,
            )
            .await?;
            Ok(NspawnConfigResult::default())
        }
        NspawnConfigOperation::Remove(request) => {
            let path = nspawn_path(&request.machine);
            remove_config_at(&path).await?;
            Ok(NspawnConfigResult::default())
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
    remove_optional_file(&crate::nspawn::sys::io::lock_path_for(path)).await?;
    remove_optional_file(&path.with_extension("lock")).await?;
    Ok(())
}

async fn write_content(path: &Path, content: String) -> Result<()> {
    AsyncLockedWriter::write_locked(path, move |_| Ok(content)).await
}

async fn write_generated_at(
    path: &Path,
    spec: &NspawnConfigSpec,
    xdg_runtime: Option<&str>,
    nvidia_state: Option<&NvidiaState>,
    invoking_uid: u32,
) -> Result<()> {
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
    write_content(path, content).await
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
    death_list: Vec<String>,
) -> Result<()> {
    validate_nvidia_update(&state, &death_list)?;
    AsyncLockedWriter::write_locked(path, move |existing| {
        let content = existing.ok_or_else(|| {
            NspawnError::Runtime(format!("No .nspawn configuration found for {machine}"))
        })?;
        let updated = NspawnConfig::apply_gpu_passthrough_to_content(content, &state, &death_list)?;
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

fn validate_nvidia_update(state: &NvidiaState, death_list: &[String]) -> Result<()> {
    if state.binds.len() > MAX_NVIDIA_BINDS || death_list.len() > MAX_NVIDIA_BINDS {
        return Err(NspawnError::Validation(
            "Too many NVIDIA bind entries".into(),
        ));
    }

    for bind in &state.binds {
        validate_absolute_value("NVIDIA host path", &bind.host_path)?;
        validate_absolute_value("NVIDIA container path", &bind.container_path)?;
    }
    for path in death_list {
        validate_absolute_value("NVIDIA removal path", path)?;
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
    }

    #[test]
    fn write_operation_contains_no_container_credentials() {
        let config = ContainerConfig {
            name: "test".into(),
            root_password: Some("secret".into()),
            ..Default::default()
        };
        let operation = NspawnConfigOperation::Write(WriteNspawnConfig {
            spec: NspawnConfigSpec::try_from(&config).unwrap(),
            xdg_runtime: None,
            nvidia_state: None,
        });
        let json = serde_json::to_string(&operation).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("root_password"));
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

        write_generated_at(&path, &spec, None, None, uzers::get_current_uid())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Boot=yes"));
        assert!(content.contains("Hostname=test-host"));
        assert!(crate::nspawn::sys::io::lock_path_for(&path).exists());
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
            private_users: Some("yes".into()),
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
        let lock_path = crate::nspawn::sys::io::lock_path_for(&path);
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
    async fn gpu_update_preserves_existing_unmanaged_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.nspawn");
        tokio::fs::write(&path, "[Exec]\nBoot=yes\n\n[Custom]\nPreserve=this\n")
            .await
            .unwrap();

        let state = NvidiaState {
            binds: vec![crate::nspawn::platform::nvidia::state::PassthroughBind {
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
