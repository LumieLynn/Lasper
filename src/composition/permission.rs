//! Permission management: elevation mode and audit scoping.
//!
//! Lasper does NOT pre-gate mutating operations — polkit handles authorization
//! at the backend level (via machinectl CLI or systemd-machined DBus).
//! The [`AuditScope`] wraps each privileged call with audit logging.
//!
//! In Elevated mode, privileged operations are dispatched through the
//! elevated daemon through closed, typed operations while keeping its main
//! loop responsive for DBus queries.

use crate::application::machine_lifecycle::{
    MachineLifecycleAction, MachineLifecycleOutcome, MachineLifecycleResult,
};
use crate::application::ImageRemovalOutcome;
use crate::config::AppSettings;
use std::fmt;
use std::future::Future;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PermissionError {
    #[error("permission operation must not be empty")]
    EmptyOperation,
}

// ── Level ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// Process is root — all operations work without a privilege launcher.
    Root,
    /// Non-root with privileged operations routed through a root daemon.
    Elevated,
    /// Non-root, no daemon — polkit handles authorization.
    User,
}

impl PermissionLevel {
    pub fn is_elevated(self) -> bool {
        matches!(self, Self::Root | Self::Elevated)
    }
}

// ── Audit scope ──

/// Scoped, single-use audit logger for one privileged operation.
///
/// Consumed on [`run`](Self::run) — cannot be reused. Carries the
/// operation name for audit logging. The actual privilege elevation
/// happens in typed daemon workers or at the backend level.
pub struct AuditScope {
    operation: String,
    _private: (),
}

impl AuditScope {
    pub(crate) async fn run<F, T>(self, f: F) -> T
    where
        F: Future<Output = T> + Send,
        T: AuditOutcome,
    {
        log::info!("[AUDIT] begin: {}", self.operation);
        let result = f.await;
        let completion = result.audit_completion();
        log::log!(
            if completion.reason.is_some() {
                log::Level::Warn
            } else {
                log::Level::Info
            },
            "[AUDIT] end:   {}{}",
            self.operation,
            completion
        );
        result
    }
}

/// Completed operations supply facts; AuditScope owns their log format and level.
pub(crate) trait AuditOutcome {
    fn audit_completion(&self) -> AuditCompletion<'_>;
}

pub(crate) struct AuditCompletion<'a> {
    status: &'static str,
    route: Option<&'static str>,
    reason: Option<&'a str>,
}

impl fmt::Display for AuditCompletion<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "; status={}", self.status)?;
        if let Some(route) = self.route {
            write!(f, "; route={route}")?;
        }
        if let Some(reason) = self.reason {
            write!(f, "; reason=")?;
            // Diagnostics can contain guest/tool output. Keep the audit entry
            // on one bounded line without hiding the operation's actual result.
            let mut chars = reason.chars();
            for ch in chars.by_ref().take(512) {
                if ch.is_control() {
                    write!(f, "{}", ch.escape_default())?;
                } else {
                    write!(f, "{ch}")?;
                }
            }
            if chars.next().is_some() {
                write!(f, " [truncated]")?;
            }
        }
        Ok(())
    }
}

impl AuditOutcome for MachineLifecycleOutcome {
    fn audit_completion(&self) -> AuditCompletion<'_> {
        let (status, reason) = match &self.result {
            MachineLifecycleResult::Succeeded
                if matches!(self.action, MachineLifecycleAction::Runtime(_)) =>
            {
                ("requested", None)
            }
            MachineLifecycleResult::Succeeded => ("succeeded", None),
            MachineLifecycleResult::NotAttempted(reason) => ("not_attempted", Some(reason)),
            MachineLifecycleResult::Rejected { reason, .. } => ("rejected", Some(reason)),
            MachineLifecycleResult::Failed(reason) => ("failed", Some(reason)),
            MachineLifecycleResult::OutcomeUnknown(reason) => ("unknown", Some(reason)),
        };
        AuditCompletion {
            status,
            route: self.route.map(|route| route.label()),
            reason: reason.map(String::as_str),
        }
    }
}

impl AuditOutcome for ImageRemovalOutcome {
    fn audit_completion(&self) -> AuditCompletion<'_> {
        let (status, reason) = match self {
            Self::Removed(_) => ("removed", None),
            Self::NotAttempted { reason, .. } => ("not_attempted", Some(reason)),
            Self::Rejected { reason, .. } => ("rejected", Some(reason)),
            Self::Failed { reason, .. } => ("failed", Some(reason)),
            Self::OutcomeUnknown { reason, .. } => ("unknown", Some(reason)),
        };
        AuditCompletion {
            status,
            route: None,
            reason: reason.map(String::as_str),
        }
    }
}

// ── Trait ──

#[async_trait::async_trait]
pub trait PermissionManager: Send + Sync + 'static {
    fn level(&self) -> PermissionLevel;
    async fn request_elevation(&self, operation: String) -> Result<AuditScope, PermissionError>;
}

// ── Default implementation ──

pub struct DefaultPermissionManager {
    level: PermissionLevel,
}

impl DefaultPermissionManager {
    pub fn new() -> Self {
        Self {
            level: if uzers::get_current_uid() == 0 {
                PermissionLevel::Root
            } else {
                PermissionLevel::User
            },
        }
    }

    /// Apply the elevation flag/config — upgrade User → Elevated.
    pub fn with_elevation(mut self, enabled: bool) -> Self {
        if enabled && self.level == PermissionLevel::User {
            self.level = PermissionLevel::Elevated;
        }
        self
    }

    pub fn wants_elevation(want_elevation_flag: bool, settings: &AppSettings) -> bool {
        want_elevation_flag || settings.elevate
    }
}

#[async_trait::async_trait]
impl PermissionManager for DefaultPermissionManager {
    fn level(&self) -> PermissionLevel {
        self.level
    }

    async fn request_elevation(&self, operation: String) -> Result<AuditScope, PermissionError> {
        if operation.trim().is_empty() {
            return Err(PermissionError::EmptyOperation);
        }
        Ok(AuditScope {
            operation,
            _private: (),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::machine_lifecycle::{MachineRejection, MachineRuntimeAction};
    use crate::application::operations::ExecutionRoute;
    use crate::domain::machine::MachineName;

    #[test]
    fn machine_audit_preserves_request_results_and_the_execution_route() {
        for (result, status) in [
            (MachineLifecycleResult::Succeeded, "requested"),
            (
                MachineLifecycleResult::NotAttempted("not launched".into()),
                "not_attempted",
            ),
            (
                MachineLifecycleResult::Rejected {
                    rejection: MachineRejection::PermissionDenied,
                    reason: "denied".into(),
                },
                "rejected",
            ),
            (MachineLifecycleResult::Failed("failed".into()), "failed"),
            (
                MachineLifecycleResult::OutcomeUnknown("lost response".into()),
                "unknown",
            ),
        ] {
            let outcome = MachineLifecycleOutcome {
                machine: MachineName::new("test").unwrap(),
                action: MachineLifecycleAction::Runtime(MachineRuntimeAction::Poweroff),
                result,
                route: Some(ExecutionRoute::DirectDbus),
                fallback: None,
            };
            let completion = outcome.audit_completion();
            let line = completion.to_string();
            assert!(line.starts_with(&format!("; status={status}; route=direct D-Bus")));
            assert_eq!(completion.reason.is_none(), status == "requested");
        }
    }

    #[test]
    fn audit_diagnostics_are_bounded_and_cannot_inject_control_sequences() {
        let reason = format!("error\n[AUDIT] forged\r\x1b[2J{}", "界".repeat(600));
        let line = AuditCompletion {
            status: "failed",
            route: None,
            reason: Some(&reason),
        }
        .to_string();

        assert!(!line.chars().any(char::is_control));
        assert!(line.contains("error\\n[AUDIT] forged\\r\\u{1b}[2J"));
        assert!(line.ends_with(" [truncated]"));
        assert!(line.chars().count() < 600);
    }

    #[test]
    fn test_wants_elevation_flag_wins() {
        let settings = AppSettings {
            elevate: false,
            ..Default::default()
        };
        assert!(DefaultPermissionManager::wants_elevation(true, &settings));
    }

    #[test]
    fn test_wants_elevation_config() {
        let settings = AppSettings {
            elevate: true,
            ..Default::default()
        };
        assert!(DefaultPermissionManager::wants_elevation(false, &settings));
    }

    #[test]
    fn test_wants_elevation_neither() {
        let settings = AppSettings::default();
        assert!(!DefaultPermissionManager::wants_elevation(false, &settings));
    }

    #[test]
    fn test_with_elevation_upgrades_user() {
        let pm = DefaultPermissionManager::new().with_elevation(true);
        assert_eq!(pm.level(), PermissionLevel::Elevated);
    }

    #[test]
    fn test_with_elevation_noop_when_false() {
        let pm = DefaultPermissionManager::new().with_elevation(false);
        assert_eq!(pm.level(), PermissionLevel::User);
    }

    #[tokio::test]
    async fn empty_audit_operation_is_rejected() {
        let pm = DefaultPermissionManager::new();
        assert!(matches!(
            pm.request_elevation("  ".into()).await,
            Err(PermissionError::EmptyOperation)
        ));
    }
}
