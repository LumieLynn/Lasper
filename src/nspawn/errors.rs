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

    #[error("Deployment cancelled")]
    DeploymentCancelled,

    #[error("Deployment cancelled; rollback incomplete: {0}")]
    DeploymentCancellationRollbackIncomplete(String),

    #[error("Deployment process state is unknown: {0}")]
    DeploymentProcessStateUnknown(String),

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

    /// Whether this error is a polkit authorization rejection from DBus.
    ///
    /// systemd-machined returns these DBus error names when polkit blocks
    /// an operation that requires `auth_self` or `auth_admin`.
    pub fn is_polkit_rejection(&self) -> bool {
        match self {
            Self::Dbus(e) => {
                let msg = e.to_string();
                msg.contains("InteractiveAuthorizationRequired")
                    || msg.contains("PolicyKit1.Error.NotAuthorized")
                    || msg.contains("PolicyKit1.Error.Failed")
                    || msg.contains("PolicyKit1.Error.AuthorizationFailed")
                    || msg.contains("DBus.Error.AccessDenied")
            }
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, NspawnError>;

impl From<crate::application::provisioning::DeploymentCancellationRequested> for NspawnError {
    fn from(_: crate::application::provisioning::DeploymentCancellationRequested) -> Self {
        Self::DeploymentCancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dbus_err(msg: &str) -> NspawnError {
        NspawnError::Dbus(zbus::Error::Failure(msg.into()))
    }

    #[test]
    fn test_is_polkit_rejection_interactive_auth() {
        let err = dbus_err(
            "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired: \
             Access denied as the requested operation requires interactive \
             authentication.",
        );
        assert!(err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_not_authorized() {
        let err = dbus_err("org.freedesktop.PolicyKit1.Error.NotAuthorized: Not authorized");
        assert!(err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_access_denied() {
        let err = dbus_err("org.freedesktop.DBus.Error.AccessDenied: Access denied");
        assert!(err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_auth_failed() {
        let err = dbus_err("org.freedesktop.PolicyKit1.Error.AuthorizationFailed: ...");
        assert!(err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_polkit_failed() {
        let err = dbus_err("org.freedesktop.PolicyKit1.Error.Failed: Action not allowed");
        assert!(err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_false_for_other_dbus_error() {
        let err = dbus_err("org.freedesktop.machine1.NoSuchMachine: No machine 'foo' known");
        assert!(!err.is_polkit_rejection());
    }

    #[test]
    fn test_is_polkit_rejection_false_for_non_dbus_error() {
        let err = NspawnError::Generic("something else".into());
        assert!(!err.is_polkit_rejection());
    }
}
