//! X11 page effects and completion events.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use super::request::{X11PageAction, X11PageEvent, X11PageUpdate};
use crate::application::configuration::ConfigurationTarget;
use crate::application::operations::HostOperationGuard;
use crate::application::x11::{X11AccessError, X11DesktopAccessError};
use crate::application::HostOperationTracker;
use crate::tui::features::configuration::executor::{PageEffect, PageEventSender};

pub(in crate::tui::features::configuration) fn start_action(
    service: Arc<crate::application::x11::X11AccessService>,
    action: X11PageAction,
    host_operations: &HostOperationTracker,
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
                target,
                host_socket,
                generation,
                events,
                host_operations.begin(),
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
                target,
                host_socket,
                record_id,
                generation,
                events,
                host_operations.begin(),
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
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: PageEventSender<X11PageEvent>,
    operation: HostOperationGuard,
) -> tokio::task::JoinHandle<()> {
    let configuration_target = ConfigurationTarget::Machine(target.machine().clone());
    tokio::spawn(async move {
        let result = finish_mutation(
            operation,
            "X11 authorization stopped unexpectedly; inspect the current ACL before retrying",
            service.authorize(target, host_socket),
        )
        .await;
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
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    record_id: String,
    generation: u64,
    events: PageEventSender<X11PageEvent>,
    operation: HostOperationGuard,
) -> tokio::task::JoinHandle<()> {
    let configuration_target = ConfigurationTarget::Machine(target.machine().clone());
    tokio::spawn(async move {
        let result = finish_mutation(
            operation,
            "X11 revocation stopped unexpectedly; inspect the current ACL before retrying",
            service.revoke(target, host_socket, record_id),
        )
        .await;
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

async fn finish_mutation<T>(
    operation: HostOperationGuard,
    panic_message: &str,
    future: impl std::future::Future<Output = Result<T, X11AccessError>>,
) -> Result<T, X11AccessError> {
    // Once accepted, the whole operation must reach its recorded outcome
    // and synchronize lifecycle activation even if its view disappears.
    let result = match AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(_) => Err(desktop_error(panic_message)),
    };
    // Publishing the UI event is separate from finishing the host operation.
    drop(operation);
    result
}

async fn send_event(events: PageEventSender<X11PageEvent>, event: X11PageEvent) {
    events.send(event).await;
}

fn desktop_error(message: &str) -> X11AccessError {
    X11AccessError::Desktop(X11DesktopAccessError::new(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test(start_paused = true)]
    async fn accepted_mutation_finishes_after_view_detach_and_the_old_deadline() {
        let tracker = HostOperationTracker::default();
        let operation = tracker.begin();
        let (finish_tx, finish_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = finish_mutation(operation, "panic sentinel", async {
                finish_rx.await.unwrap();
                Ok(42)
            })
            .await;
            let _ = result_tx.send(result);
        });
        drop(task);
        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(tracker.active_count(), 1);
        assert!(!result_rx.is_terminated());
        finish_tx.send(()).unwrap();
        assert_eq!(result_rx.await.unwrap().unwrap(), 42);
        assert_eq!(tracker.active_count(), 0);
    }

    #[tokio::test]
    async fn unexpected_mutation_failure_releases_the_operation_and_requires_inspection() {
        let tracker = HostOperationTracker::default();
        let result: Result<(), _> = finish_mutation(
            tracker.begin(),
            "X11 authorization stopped unexpectedly; inspect the current ACL before retrying",
            async { panic!("panic sentinel") },
        )
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("inspect the current ACL before retrying"));
        assert_eq!(tracker.active_count(), 0);
    }
}
