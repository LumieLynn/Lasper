use crate::adapters::elevated::ElevatedDaemon;
use crate::composition::PermissionLevel;
use std::sync::Arc;

/// Validated process authority used only while the production service graph is
/// assembled. It carries no host stores and never enters application or UI state.
pub(crate) enum CompositionMode {
    User,
    Root,
    Elevated(Arc<ElevatedDaemon>),
}

impl CompositionMode {
    pub(crate) fn new(
        level: PermissionLevel,
        daemon: Option<Arc<ElevatedDaemon>>,
    ) -> Result<Self, CompositionModeError> {
        validate_mode(level, daemon.is_some())?;
        Ok(match level {
            PermissionLevel::User => Self::User,
            PermissionLevel::Root => Self::Root,
            PermissionLevel::Elevated => {
                Self::Elevated(daemon.expect("validated elevated composition mode has a daemon"))
            }
        })
    }

    pub(crate) fn permission_level(&self) -> PermissionLevel {
        match self {
            Self::User => PermissionLevel::User,
            Self::Root => PermissionLevel::Root,
            Self::Elevated(_) => PermissionLevel::Elevated,
        }
    }

    pub(crate) fn daemon(&self) -> Option<&Arc<ElevatedDaemon>> {
        match self {
            Self::Elevated(daemon) => Some(daemon),
            Self::User | Self::Root => None,
        }
    }
}

fn validate_mode(level: PermissionLevel, has_daemon: bool) -> Result<(), CompositionModeError> {
    match (level, has_daemon) {
        (PermissionLevel::Elevated, false) => Err(CompositionModeError::MissingElevatedDaemon),
        (PermissionLevel::Root | PermissionLevel::User, true) => {
            Err(CompositionModeError::UnexpectedElevatedDaemon { level })
        }
        _ => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CompositionModeError {
    #[error("elevated execution requires an elevated daemon")]
    MissingElevatedDaemon,
    #[error("{level:?} execution must not receive an elevated daemon")]
    UnexpectedElevatedDaemon { level: PermissionLevel },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_mode_requires_exact_daemon_pairing() {
        assert!(validate_mode(PermissionLevel::User, false).is_ok());
        assert!(validate_mode(PermissionLevel::Root, false).is_ok());
        assert!(matches!(
            validate_mode(PermissionLevel::Elevated, false),
            Err(CompositionModeError::MissingElevatedDaemon)
        ));
        assert!(matches!(
            validate_mode(PermissionLevel::User, true),
            Err(CompositionModeError::UnexpectedElevatedDaemon { .. })
        ));
        assert!(matches!(
            validate_mode(PermissionLevel::Root, true),
            Err(CompositionModeError::UnexpectedElevatedDaemon { .. })
        ));
        assert!(validate_mode(PermissionLevel::Elevated, true).is_ok());
    }
}
