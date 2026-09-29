//! X11 page effects and completion events.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::mpsc;

use super::{X11PageAction, X11PageUpdate};
use crate::application::configuration::ConfigurationTarget;
use crate::application::x11::{
    X11AccessCheck, X11AccessError, X11Authorization, X11AuthorizationDisposition,
    X11DesktopAccessError, X11Revocation, X11RevocationDisposition,
};
use crate::tui::configuration::pages::{ConfigurationPageEvent, ConfigurationPageUpdate};
use crate::tui::configuration::ConfigurationPageEffect;
use crate::tui::events::AppEvent;
use crate::tui::StatusLevel;

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

pub(in crate::tui::configuration) fn start_action(
    service: Arc<crate::application::x11::X11AccessService>,
    action: X11PageAction,
    events: Option<mpsc::Sender<AppEvent>>,
) -> ConfigurationPageEffect {
    let update = match action {
        X11PageAction::Check {
            generation,
            target,
            host_socket,
        } => match events {
            Some(events) => X11PageUpdate::TrackCheck(spawn_check(
                service,
                ConfigurationTarget::Machine(target.machine().clone()),
                target,
                host_socket,
                generation,
                events,
            )),
            None => X11PageUpdate::Checked {
                generation,
                result: Err(desktop_error("Application event channel is unavailable")),
            },
        },
        X11PageAction::Authorize {
            generation,
            target,
            host_socket,
        } => match events {
            Some(events) => X11PageUpdate::TrackAuthorization(spawn_authorization(
                service,
                ConfigurationTarget::Machine(target.machine().clone()),
                target,
                host_socket,
                generation,
                events,
            )),
            None => X11PageUpdate::Authorized {
                generation,
                result: Err(desktop_error(
                    "Application event channel is unavailable; no authorization was attempted",
                )),
            },
        },
        X11PageAction::Revoke {
            generation,
            target,
            host_socket,
            record_id,
        } => match events {
            Some(events) => X11PageUpdate::TrackRevocation(spawn_revocation(
                service,
                ConfigurationTarget::Machine(target.machine().clone()),
                target,
                host_socket,
                record_id,
                generation,
                events,
            )),
            None => X11PageUpdate::Revoked {
                generation,
                result: Err(desktop_error(
                    "Application event channel is unavailable; no revocation was attempted",
                )),
            },
        },
    };
    ConfigurationPageEffect::Update(ConfigurationPageUpdate::X11(Box::new(update)))
}

fn spawn_check(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.check(target, host_socket)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(desktop_error("X11 access check stopped unexpectedly")),
            Err(_) => Err(desktop_error("X11 access check timed out")),
        };
        send_event(
            events,
            X11PageEvent::Checked {
                generation,
                target: configuration_target,
                result,
            },
        )
        .await;
    })
}

fn spawn_authorization(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.authorize(target, host_socket)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(desktop_error(
                "X11 authorization stopped unexpectedly; inspect the current ACL before retrying",
            )),
            Err(_) => Err(desktop_error(
                "X11 authorization timed out; inspect the current ACL before retrying",
            )),
        };
        send_event(
            events,
            X11PageEvent::Authorized {
                generation,
                target: configuration_target,
                result,
            },
        )
        .await;
    })
}

fn spawn_revocation(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    record_id: String,
    generation: u64,
    events: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.revoke(target, host_socket, record_id)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(desktop_error(
                "X11 revocation stopped unexpectedly; inspect the current ACL before retrying",
            )),
            Err(_) => Err(desktop_error(
                "X11 revocation timed out; inspect the current ACL before retrying",
            )),
        };
        send_event(
            events,
            X11PageEvent::Revoked {
                generation,
                target: configuration_target,
                result,
            },
        )
        .await;
    })
}

async fn send_event(events: mpsc::Sender<AppEvent>, event: X11PageEvent) {
    let _ = events
        .send(AppEvent::ConfigurationPage(ConfigurationPageEvent::X11(
            Box::new(event),
        )))
        .await;
}

fn desktop_error(message: &str) -> X11AccessError {
    X11AccessError::Desktop(X11DesktopAccessError::new(message))
}
