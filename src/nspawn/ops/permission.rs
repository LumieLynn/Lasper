//! Permission management: elevation mode and audit scoping.
//!
//! Lasper does NOT pre-gate mutating operations — polkit handles authorization
//! at the backend level (via machinectl CLI or systemd-machined DBus).
//! The [`AuditScope`] wraps each privileged call with audit logging.
//!
//! In Elevated mode, privileged operations are dispatched through the
//! elevated daemon which spawns one-shot workers for command execution
//! and file I/O while keeping its main loop responsive for DBus queries.

use crate::config::AppSettings;
use crate::nspawn::errors::Result;
use std::future::Future;

// ── Level ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// Process is root — all operations work without sudo.
    Root,
    /// Non-root with sudo wrapping for privileged commands (`-e` flag).
    Elevated,
    /// Non-root, no sudo — polkit handles authorization.
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
/// happens in the daemon's one-shot workers or at the backend level.
pub struct AuditScope {
    operation: String,
    _private: (),
}

impl AuditScope {
    pub async fn run<F, T>(self, f: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send,
    {
        log::info!("[AUDIT] begin: {}", self.operation);
        let result = f.await;
        log::info!("[AUDIT] end:   {}", self.operation);
        result
    }
}

// ── Trait ──

#[async_trait::async_trait]
pub trait PermissionManager: Send + Sync + 'static {
    fn level(&self) -> PermissionLevel;
    async fn request_elevation(&self, operation: String) -> Result<AuditScope>;
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
    pub fn with_elevation(mut self, use_sudo: bool) -> Self {
        if use_sudo && self.level == PermissionLevel::User {
            self.level = PermissionLevel::Elevated;
        }
        self
    }

    pub fn wants_elevation(want_elevation_flag: bool, settings: &Option<AppSettings>) -> bool {
        want_elevation_flag || settings.as_ref().map(|s| s.elevate).unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl PermissionManager for DefaultPermissionManager {
    fn level(&self) -> PermissionLevel {
        self.level
    }

    async fn request_elevation(&self, operation: String) -> Result<AuditScope> {
        Ok(AuditScope {
            operation,
            _private: (),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wants_elevation_flag_wins() {
        let settings = Some(AppSettings {
            elevate: false,
            ..Default::default()
        });
        assert!(DefaultPermissionManager::wants_elevation(true, &settings));
    }

    #[test]
    fn test_wants_elevation_config() {
        let settings = Some(AppSettings {
            elevate: true,
            ..Default::default()
        });
        assert!(DefaultPermissionManager::wants_elevation(false, &settings));
    }

    #[test]
    fn test_wants_elevation_neither() {
        let settings: Option<AppSettings> = None;
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
}
