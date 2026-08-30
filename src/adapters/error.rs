use std::path::PathBuf;
use thiserror::Error;

/// Transitional host-adapter error.
///
/// This type remains intentionally private to adapters while individual
/// adapter contracts migrate to narrower error enums.  It is no longer a
/// shared `nspawn` model and must not be imported by application or TUI code.
#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum NspawnError {
    #[error("Permission denied: root privileges required")]
    PermissionDenied,

    #[error("Command Failed ({0}): {1}. Output: {2}")]
    CommandFailed(String, String, String),

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

    #[error("Image '{0}' is protected and cannot be removed")]
    ProtectedImage(String),

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

    #[error("Deployment rollback incomplete: {0}")]
    DeploymentRollbackIncomplete(String),

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
    pub(crate) fn cmd_failed(
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

    /// Whether this error is a polkit authorization rejection from D-Bus.
    pub(crate) fn is_polkit_rejection(&self) -> bool {
        match self {
            Self::Dbus(error) => is_permission_dbus_error(error),
            _ => false,
        }
    }
}

pub(crate) fn is_permission_dbus_error_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.AccessDenied"
            | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired"
            | "org.freedesktop.PolicyKit1.Error.NotAuthorized"
            | "org.freedesktop.PolicyKit1.Error.AuthorizationFailed"
            | "org.freedesktop.PolicyKit1.Error.Failed"
            | "System.Error.EACCES"
            | "System.Error.EPERM"
    )
}

fn is_permission_dbus_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => is_permission_dbus_error_name(name.as_str()),
        zbus::Error::FDO(error) => match error.as_ref() {
            zbus::fdo::Error::AccessDenied(_) => true,
            zbus::fdo::Error::ZBus(error) => is_permission_dbus_error(error),
            _ => false,
        },
        _ => false,
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

    #[test]
    fn permission_errors_are_classified_by_dbus_name() {
        for name in [
            "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired",
            "org.freedesktop.PolicyKit1.Error.NotAuthorized",
            "org.freedesktop.DBus.Error.AccessDenied",
            "org.freedesktop.PolicyKit1.Error.AuthorizationFailed",
            "org.freedesktop.PolicyKit1.Error.Failed",
            "System.Error.EACCES",
            "System.Error.EPERM",
        ] {
            assert!(is_permission_dbus_error_name(name));
        }
        assert!(!is_permission_dbus_error_name(
            "org.freedesktop.machine1.NoSuchMachine"
        ));
    }

    #[test]
    fn typed_fdo_access_denied_is_a_permission_rejection() {
        let error = NspawnError::Dbus(zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied(
            "not authorized".into(),
        ))));
        assert!(error.is_polkit_rejection());
    }

    #[test]
    fn human_readable_error_text_does_not_drive_permission_classification() {
        let error = NspawnError::Dbus(zbus::Error::Failure(
            "org.freedesktop.DBus.Error.AccessDenied: access denied".into(),
        ));
        assert!(!error.is_polkit_rejection());
    }
}
