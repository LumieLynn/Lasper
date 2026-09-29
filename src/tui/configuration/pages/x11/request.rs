//! X11 page messages and status reporting. No workspace-specific routing.

use crate::application::configuration::ConfigurationTarget;
use crate::application::sessions::ShellTarget;
use crate::application::x11::{
    X11AccessCheck, X11AccessError, X11Authorization, X11AuthorizationDisposition, X11Revocation,
    X11RevocationDisposition,
};
use crate::domain::x11::HostX11Socket;
use crate::tui::StatusLevel;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum X11PageAction {
    Check {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
    },
    Authorize {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
    },
    Revoke {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
        record_id: String,
    },
}

pub(crate) enum X11PageUpdate {
    TrackCheck(tokio::task::JoinHandle<()>),
    TrackAuthorization(tokio::task::JoinHandle<()>),
    TrackRevocation(tokio::task::JoinHandle<()>),
    Checked {
        generation: u64,
        result: Result<X11AccessCheck, X11AccessError>,
    },
    Authorized {
        generation: u64,
        result: Result<X11Authorization, X11AccessError>,
    },
    Revoked {
        generation: u64,
        result: Result<X11Revocation, X11AccessError>,
    },
}

#[derive(Debug)]
pub(crate) enum X11PageEvent {
    Checked {
        generation: u64,
        target: ConfigurationTarget,
        result: Result<X11AccessCheck, X11AccessError>,
    },
    Authorized {
        generation: u64,
        target: ConfigurationTarget,
        result: Result<X11Authorization, X11AccessError>,
    },
    Revoked {
        generation: u64,
        target: ConfigurationTarget,
        result: Result<X11Revocation, X11AccessError>,
    },
}

impl X11PageEvent {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Checked { .. } => "configuration-x11-checked",
            Self::Authorized { .. } => "configuration-x11-authorized",
            Self::Revoked { .. } => "configuration-x11-revoked",
        }
    }

    pub(crate) fn target(&self) -> &ConfigurationTarget {
        match self {
            Self::Checked { target, .. }
            | Self::Authorized { target, .. }
            | Self::Revoked { target, .. } => target,
        }
    }

    pub(crate) fn into_update(self) -> X11PageUpdate {
        match self {
            Self::Checked {
                generation, result, ..
            } => X11PageUpdate::Checked { generation, result },
            Self::Authorized {
                generation, result, ..
            } => X11PageUpdate::Authorized { generation, result },
            Self::Revoked {
                generation, result, ..
            } => X11PageUpdate::Revoked { generation, result },
        }
    }

    pub(crate) fn detached_status(self) -> Option<(String, StatusLevel)> {
        match self {
            Self::Checked { .. } => None,
            Self::Authorized { result, .. } => Some(match result {
                Ok(authorization) => {
                    let message = match authorization.disposition() {
                        X11AuthorizationDisposition::Added { .. } => {
                            "X11 access authorized; the operation record was saved"
                        }
                        X11AuthorizationDisposition::PreExisting => {
                            "X11 access was already present; no Lasper ownership was recorded"
                        }
                        X11AuthorizationDisposition::AccessControlDisabled => {
                            "X11 access control is disabled; no ACL entry was added"
                        }
                    };
                    (message.to_owned(), StatusLevel::Success)
                }
                Err(error) => (error.to_string(), StatusLevel::Error),
            }),
            Self::Revoked { result, .. } => Some(match result {
                Ok(revocation) => {
                    let message = match revocation.disposition() {
                        X11RevocationDisposition::Revoked { .. } => {
                            "X11 access revoked; the operation record was retained"
                        }
                        X11RevocationDisposition::AlreadyAbsent { .. } => {
                            "X11 access was already absent; the operation record was finalized"
                        }
                    };
                    (message.to_owned(), StatusLevel::Success)
                }
                Err(error) => (error.to_string(), StatusLevel::Error),
            }),
        }
    }
}
