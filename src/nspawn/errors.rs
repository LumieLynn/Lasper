use thiserror::Error;
use std::path::PathBuf;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum NspawnError {
    #[error("Permission denied: root privileges required")]
    PermissionDenied,

    #[error("Command '{0}' failed: {1}")]
    CommandFailed(String, String),

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

    #[error("Tool '{0}' not found on PATH")]
    ToolNotFound(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Deployment failed: {0}")]
    DeployError(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, NspawnError>;
