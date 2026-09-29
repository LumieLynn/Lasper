//! Wayland page effects and completion events.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use super::request::{WaylandPageAction, WaylandPageEvent, WaylandPageUpdate};
use super::PAGE_ID;
use crate::application::configuration::ConfigurationTarget;
use crate::application::sessions::{SessionError, WaylandShellRequest};
use crate::tui::configuration::executor::{
    ConfigurationTerminalRequest, PageEffect, PageEventSender,
};

pub(in crate::tui::configuration) fn start_action(
    service: Arc<crate::application::sessions::SessionService>,
    action: WaylandPageAction,
    events: Option<PageEventSender<WaylandPageEvent>>,
) -> PageEffect<WaylandPageUpdate> {
    match action {
        WaylandPageAction::Check {
            generation,
            target,
            host_socket,
        } => {
            let update = match events {
                Some(events) => WaylandPageUpdate::TrackCheck(spawn_check(
                    service,
                    ConfigurationTarget::Machine(target.machine().clone()),
                    target,
                    host_socket,
                    generation,
                    events,
                )),
                None => WaylandPageUpdate::Checked {
                    generation,
                    result: Err(SessionError::new(
                        "Application event channel is unavailable",
                    )),
                },
            };
            PageEffect::Update(update)
        }
        WaylandPageAction::EnterShell {
            target,
            host_socket,
        } => PageEffect::OpenTerminal(ConfigurationTerminalRequest::new(
            PAGE_ID,
            target,
            WaylandShellRequest::SelectedHostDisplay(host_socket),
        )),
    }
}

fn spawn_check(
    service: Arc<crate::application::sessions::SessionService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::wayland::HostWaylandSocket,
    generation: u64,
    events: PageEventSender<WaylandPageEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.test_wayland(target, host_socket)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SessionError::new(
                "Wayland access check stopped unexpectedly",
            )),
            Err(_) => Err(SessionError::new("Wayland access check timed out")),
        };
        events
            .send(WaylandPageEvent::Checked {
                generation,
                target: configuration_target,
                result,
            })
            .await;
    })
}
