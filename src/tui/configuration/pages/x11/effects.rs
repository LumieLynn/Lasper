//! X11 page effects and completion events.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use super::request::{X11PageAction, X11PageEvent, X11PageUpdate};
use crate::application::configuration::ConfigurationTarget;
use crate::application::x11::{X11AccessError, X11DesktopAccessError};
use crate::tui::configuration::executor::{PageEffect, PageEventSender};

pub(in crate::tui::configuration) fn start_action(
    service: Arc<crate::application::x11::X11AccessService>,
    action: X11PageAction,
    events: Option<PageEventSender<X11PageEvent>>,
) -> PageEffect<X11PageUpdate> {
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
    PageEffect::Update(update)
}

fn spawn_check(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: PageEventSender<X11PageEvent>,
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
    events: PageEventSender<X11PageEvent>,
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
    events: PageEventSender<X11PageEvent>,
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

async fn send_event(events: PageEventSender<X11PageEvent>, event: X11PageEvent) {
    events.send(event).await;
}

fn desktop_error(message: &str) -> X11AccessError {
    X11AccessError::Desktop(X11DesktopAccessError::new(message))
}
