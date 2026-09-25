//! Requests, results, and the caller-owned desktop ACL port.

use crate::application::sessions::{SessionError, ShellTarget, X11ProjectionContext};
use crate::domain::x11::HostX11Socket;

use super::{X11AccessCheck, X11DesktopObservation};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11AuthorizationRequest {
    target: ShellTarget,
    projection: X11ProjectionContext,
    purpose: X11AuthorizationPurpose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum X11AuthorizationPurpose {
    Manual,
    ExplicitSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11RevokeRequest {
    target: ShellTarget,
    projection: X11ProjectionContext,
    record_id: String,
}

impl X11RevokeRequest {
    pub(crate) fn new(
        target: ShellTarget,
        projection: X11ProjectionContext,
        record_id: String,
    ) -> Self {
        Self {
            target,
            projection,
            record_id,
        }
    }

    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub(crate) fn record_id(&self) -> &str {
        &self.record_id
    }
}

impl X11AuthorizationRequest {
    pub(crate) fn new(target: ShellTarget, projection: X11ProjectionContext) -> Self {
        Self {
            target,
            projection,
            purpose: X11AuthorizationPurpose::Manual,
        }
    }

    pub(crate) fn for_explicit_session(
        target: ShellTarget,
        projection: X11ProjectionContext,
    ) -> Self {
        Self {
            target,
            projection,
            purpose: X11AuthorizationPurpose::ExplicitSession,
        }
    }

    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) fn projection(&self) -> &X11ProjectionContext {
        &self.projection
    }

    pub(crate) const fn purpose(&self) -> X11AuthorizationPurpose {
        self.purpose
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11AuthorizationDisposition {
    AccessControlDisabled,
    PreExisting,
    Added { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11RevocationDisposition {
    Revoked { record_id: String },
    AlreadyAbsent { record_id: String },
}

/// Result of one bounded machine-lifecycle reconcile pass for one live X11
/// endpoint. A pending ID means the machine registration has disappeared but
/// the grace/revalidation state is not yet sufficient to revoke its ACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11ReconcileReport {
    display: u16,
    revoked_record_ids: Vec<String>,
    pending_record_ids: Vec<String>,
    diagnostics: Vec<String>,
}

impl X11ReconcileReport {
    pub(crate) fn new(
        display: u16,
        revoked_record_ids: Vec<String>,
        pending_record_ids: Vec<String>,
        diagnostics: Vec<String>,
    ) -> Self {
        Self {
            display,
            revoked_record_ids,
            pending_record_ids,
            diagnostics,
        }
    }

    pub(crate) const fn display(&self) -> u16 {
        self.display
    }

    pub(crate) fn revoked_record_ids(&self) -> &[String] {
        &self.revoked_record_ids
    }

    pub(crate) fn pending_record_ids(&self) -> &[String] {
        &self.pending_record_ids
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.pending_record_ids.is_empty() && self.diagnostics.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11DesktopAuthorization {
    pub(super) observation: X11DesktopObservation,
    pub(super) disposition: X11AuthorizationDisposition,
}

impl X11DesktopAuthorization {
    pub(crate) fn new(
        observation: X11DesktopObservation,
        disposition: X11AuthorizationDisposition,
    ) -> Self {
        Self {
            observation,
            disposition,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11DesktopRevocation {
    pub(super) observation: X11DesktopObservation,
    pub(super) disposition: X11RevocationDisposition,
}

impl X11DesktopRevocation {
    pub(crate) fn new(
        observation: X11DesktopObservation,
        disposition: X11RevocationDisposition,
    ) -> Self {
        Self {
            observation,
            disposition,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11Authorization {
    pub(super) check: X11AccessCheck,
    pub(super) disposition: X11AuthorizationDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11Revocation {
    pub(super) check: X11AccessCheck,
    pub(super) disposition: X11RevocationDisposition,
}

impl X11Revocation {
    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11RevocationDisposition {
        &self.disposition
    }
}

impl X11Authorization {
    pub fn check(&self) -> &X11AccessCheck {
        &self.check
    }

    pub fn disposition(&self) -> &X11AuthorizationDisposition {
        &self.disposition
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct X11DesktopAccessError {
    message: String,
}

impl X11DesktopAccessError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum X11AccessError {
    #[error("X11 display selection failed: {0}")]
    Selection(String),
    #[error("{0}")]
    Projection(#[source] SessionError),
    #[error("X11 desktop access operation failed: {0}")]
    Desktop(#[source] X11DesktopAccessError),
}

#[async_trait::async_trait]
pub(crate) trait X11DesktopAccessPort: Send + Sync {
    async fn snapshot(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError>;

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError>;

    async fn revoke(
        &self,
        request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError>;

    async fn reconcile(
        &self,
        _socket: &HostX11Socket,
    ) -> Result<X11ReconcileReport, X11DesktopAccessError> {
        Err(X11DesktopAccessError::new(
            "X11 lifecycle reconcile is not available on this desktop access adapter",
        ))
    }

    async fn synchronize_reconcile_activation(&self) -> Result<(), X11DesktopAccessError> {
        Err(X11DesktopAccessError::new(
            "X11 lifecycle activation is not available on this desktop access adapter",
        ))
    }
}
