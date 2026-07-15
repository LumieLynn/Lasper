use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_INJECTION_FILE_BYTES: usize = 1024 * 1024;

/// Typed access to short-lived host staging files used for NVIDIA container
/// setup. The daemon derives all host paths; callers only receive a path for
/// `systemd-nspawn --bind`.
#[derive(Clone)]
pub struct NvidiaStagingStore {
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl NvidiaStagingStore {
    pub fn new(daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self { daemon }
    }

    pub async fn create_injection_file(
        &self,
        name: &str,
        kind: NvidiaInjectionFileKind,
        content: &str,
    ) -> Result<NvidiaInjectionFile> {
        let result = self
            .execute(NvidiaStagingOperation::CreateInjectionFile(
                CreateNvidiaInjectionFile {
                    machine: parse_machine_name(name)?,
                    kind,
                    content: content.to_string(),
                },
            ))
            .await?;
        result
            .file
            .ok_or_else(|| NspawnError::Runtime("NVIDIA staging operation returned no file".into()))
    }

    pub async fn remove_injection_file(
        &self,
        name: &str,
        file: &NvidiaInjectionFile,
    ) -> Result<()> {
        self.execute(NvidiaStagingOperation::RemoveInjectionFile(
            RemoveNvidiaInjectionFile {
                machine: parse_machine_name(name)?,
                kind: file.kind,
                id: file.id.clone(),
            },
        ))
        .await?;
        Ok(())
    }

    async fn execute(&self, operation: NvidiaStagingOperation) -> Result<NvidiaStagingResult> {
        if let Some(daemon) = &self.daemon {
            daemon
                .nvidia_staging(operation)
                .await
                .map_err(|error| NspawnError::Runtime(error.to_string()))
        } else {
            execute_nvidia_staging_operation(operation).await
        }
    }
}

impl std::fmt::Debug for NvidiaStagingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaStagingStore")
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum NvidiaStagingOperation {
    CreateInjectionFile(CreateNvidiaInjectionFile),
    RemoveInjectionFile(RemoveNvidiaInjectionFile),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateNvidiaInjectionFile {
    machine: MachineName,
    kind: NvidiaInjectionFileKind,
    content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveNvidiaInjectionFile {
    machine: MachineName,
    kind: NvidiaInjectionFileKind,
    id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NvidiaInjectionFileKind {
    LdConfig,
    Environment,
}

impl NvidiaInjectionFileKind {
    fn as_filename_part(self) -> &'static str {
        match self {
            Self::LdConfig => "ld",
            Self::Environment => "env",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NvidiaInjectionFile {
    pub kind: NvidiaInjectionFileKind,
    pub id: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NvidiaStagingResult {
    file: Option<NvidiaInjectionFile>,
}

pub(crate) async fn execute_nvidia_staging_operation(
    operation: NvidiaStagingOperation,
) -> Result<NvidiaStagingResult> {
    match operation {
        NvidiaStagingOperation::CreateInjectionFile(request) => {
            validate_content(&request.content)?;
            let id = uuid::Uuid::new_v4().to_string();
            let path = injection_path(&request.machine, request.kind, &id)?;
            write_injection_file(&path, &request.content)?;
            Ok(NvidiaStagingResult {
                file: Some(NvidiaInjectionFile {
                    kind: request.kind,
                    id,
                    path: path
                        .to_str()
                        .ok_or_else(|| {
                            NspawnError::Validation("NVIDIA staging path is not valid UTF-8".into())
                        })?
                        .to_string(),
                }),
            })
        }
        NvidiaStagingOperation::RemoveInjectionFile(request) => {
            validate_staging_id(&request.id)?;
            let path = injection_path(&request.machine, request.kind, &request.id)?;
            remove_optional_file(&path).await?;
            Ok(NvidiaStagingResult::default())
        }
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn injection_path(
    machine: &MachineName,
    kind: NvidiaInjectionFileKind,
    id: &str,
) -> Result<PathBuf> {
    validate_staging_id(id)?;
    Ok(PathBuf::from(format!(
        "/tmp/lasper-nvidia-{}-{}-{}.conf",
        kind.as_filename_part(),
        machine.as_str(),
        id
    )))
}

fn validate_staging_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map_err(|error| {
        NspawnError::Validation(format!("Invalid NVIDIA staging file id: {error}"))
    })?;
    Ok(())
}

fn validate_content(content: &str) -> Result<()> {
    if content.len() > MAX_INJECTION_FILE_BYTES || content.contains('\0') {
        return Err(NspawnError::Validation(
            "Invalid NVIDIA injection file content".into(),
        ));
    }
    Ok(())
}

fn write_injection_file(path: &Path, content: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    file.write_all(content.as_bytes())
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    file.sync_all()
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    Ok(())
}

async fn remove_optional_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_deserialization_rejects_invalid_machine_name() {
        let json = r#"{
            "operation": "create_injection_file",
            "params": {
                "machine": "../escape",
                "kind": "ld_config",
                "content": "/usr/lib\n"
            }
        }"#;
        assert!(serde_json::from_str::<NvidiaStagingOperation>(json).is_err());
    }

    #[tokio::test]
    async fn remove_rejects_non_uuid_id() {
        let operation = NvidiaStagingOperation::RemoveInjectionFile(RemoveNvidiaInjectionFile {
            machine: MachineName::new("test").unwrap(),
            kind: NvidiaInjectionFileKind::Environment,
            id: "../escape".into(),
        });
        let result = execute_nvidia_staging_operation(operation).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_and_remove_injection_file_round_trip() {
        let operation = NvidiaStagingOperation::CreateInjectionFile(CreateNvidiaInjectionFile {
            machine: MachineName::new("test").unwrap(),
            kind: NvidiaInjectionFileKind::LdConfig,
            content: "/usr/lib\n".into(),
        });

        let file = execute_nvidia_staging_operation(operation)
            .await
            .unwrap()
            .file
            .unwrap();
        let path = PathBuf::from(&file.path);

        assert!(path.starts_with("/tmp"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "/usr/lib\n"
        );

        execute_nvidia_staging_operation(NvidiaStagingOperation::RemoveInjectionFile(
            RemoveNvidiaInjectionFile {
                machine: MachineName::new("test").unwrap(),
                kind: file.kind,
                id: file.id,
            },
        ))
        .await
        .unwrap();

        assert!(!path.exists());
    }
}
