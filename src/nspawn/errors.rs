use std::path::PathBuf;
use thiserror::Error;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum NspawnError {
    #[error("Permission denied: root privileges required")]
    PermissionDenied,

    #[error("Command Failed ({0}): {1}. Output: {2}")]
    CommandFailed(String, String, String), // Context, Command, Error Output

    #[error("{0}")]
    Generic(String),

    #[error("IO error: {0}")]
    GenericIo(#[from] std::io::Error),

    #[error("IO error in {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),

    #[error("Container '{0}' not found")]
    ContainerNotFound(String),

    #[error("Container '{0}' is already running")]
    ContainerAlreadyRunning(String),

    #[error("Container '{0}' is not running")]
    ContainerNotRunning(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Tool '{0}' not found on PATH")]
    ToolNotFound(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Deployment failed: {0}")]
    DeployError(String),

    #[error("DBus error: {0}")]
    Dbus(#[from] zbus::Error),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("Runtime error: {0}")]
    Runtime(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl NspawnError {
    pub fn cmd_failed(
        context: impl Into<String>,
        cmd: impl Into<String>,
        output: &std::process::Output,
    ) -> Self {
        Self::CommandFailed(
            context.into(),
            cmd.into(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )
    }

    pub fn mount_failed(msg: impl Into<String>) -> Self {
        Self::StorageError(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, NspawnError>;
