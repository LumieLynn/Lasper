//! Stop request admission, drain handoffs, then cancel jobs and end sessions.

use crate::daemon::server::DaemonServerState;
use std::sync::Arc;

/// Both request channels enter through this gate before running an operation.
/// Shutdown closes it synchronously; accepted requests retain a lease until
/// their work is complete or registered with the job/session owner.
#[derive(Default)]
pub(crate) struct DaemonShutdown {
    state: parking_lot::Mutex<RequestState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct RequestState {
    requested: bool,
    active: usize,
}

pub(crate) struct ActiveRequest {
    shutdown: Arc<DaemonShutdown>,
}

impl DaemonShutdown {
    pub(crate) fn admit(self: &Arc<Self>) -> Option<ActiveRequest> {
        let mut state = self.state.lock();
        if state.requested {
            return None;
        }
        state.active += 1;
        Some(ActiveRequest {
            shutdown: Arc::clone(self),
        })
    }

    /// Any disconnect or explicit exit can request shutdown. Only the main
    /// server flow performs cleanup and exits the process.
    pub(crate) fn begin(&self) {
        self.state.lock().requested = true;
        self.changed.notify_waiters();
    }

    pub(crate) async fn requested(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().requested {
                return;
            }
            changed.await;
        }
    }

    async fn drain_requests(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().active == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        let mut state = self.shutdown.state.lock();
        state.active -= 1;
        if state.active == 0 {
            self.shutdown.changed.notify_waiters();
        }
    }
}

const DAEMON_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(750);
const DEPLOYMENT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

pub(crate) async fn shutdown_daemon_resources(server_state: &DaemonServerState) {
    server_state.shutdown.begin();
    // Accepted requests may still be preparing an artifact FD or starting a
    // session in a blocking worker. Do not cancel those futures: wait until
    // every handoff is complete before taking either registry's snapshot.
    server_state.shutdown.drain_requests().await;

    if !server_state
        .deployments
        .cancel_all_and_wait(DEPLOYMENT_SHUTDOWN_GRACE)
        .await
    {
        log::error!(
            "Daemon: provisioning jobs did not reach a terminal state before shutdown; child parent-death guards remain armed"
        );
    }

    let sessions = &server_state.sessions;
    let processes = sessions.processes();
    let mut needs_session_grace = false;
    for process in &processes {
        match sessions.signal(process, libc::SIGTERM) {
            Ok(true) => needs_session_grace = true,
            Ok(false) => {}
            Err(error) => {
                needs_session_grace = true;
                log::warn!(
                    "Daemon: failed to terminate child process group {}: {error}",
                    process.pid()
                );
            }
        }
    }

    if !needs_session_grace {
        return;
    }

    if sessions.wait_for_empty(DAEMON_SHUTDOWN_GRACE).await {
        return;
    }

    for process in sessions.processes() {
        if let Err(error) = sessions.signal(&process, libc::SIGKILL) {
            log::warn!(
                "Daemon: failed to kill child process group {}: {error}",
                process.pid()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::session::WireSessionId;
    use std::os::unix::process::CommandExt;

    #[tokio::test]
    async fn shutdown_closes_admission_and_drains_every_accepted_request() {
        let state = Arc::new(DaemonServerState::default());
        let first = state.shutdown.admit().unwrap();
        let second = state.shutdown.admit().unwrap();
        let cleanup = Arc::clone(&state);
        let shutdown = tokio::spawn(async move { shutdown_daemon_resources(&cleanup).await });

        state.shutdown.requested().await;
        assert!(state.shutdown.admit().is_none());
        assert!(!shutdown.is_finished());
        // Repeated disconnect notifications must neither reopen admission nor
        // allow cleanup to pass a handoff that still owns a request lease.
        state.shutdown.begin();
        drop(first);
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());

        drop(second);
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(state.shutdown.admit().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_includes_a_session_registered_by_an_accepted_handoff() {
        let state = Arc::new(DaemonServerState::default());
        let handoff = state.shutdown.admit().unwrap();
        let cleanup = Arc::clone(&state);
        let shutdown = tokio::spawn(async move { shutdown_daemon_resources(&cleanup).await });
        state.shutdown.requested().await;

        let id = WireSessionId::new(3).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sleep");
        command.arg("30").process_group(0);
        let mut child = command.spawn().unwrap();
        let process = state.sessions.register(id, child.id()).unwrap();
        drop(handoff);

        shutdown.await.unwrap();
        assert!(!child.wait().unwrap().success());
        state.sessions.finish(id, &process);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_without_live_sessions_skips_the_grace_period() {
        let state = DaemonServerState::default();
        let started = tokio::time::Instant::now();

        shutdown_daemon_resources(&state).await;

        assert_eq!(tokio::time::Instant::now(), started);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_with_a_live_session_preserves_the_grace_period() {
        let state = DaemonServerState::default();
        let id = WireSessionId::new(1).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sleep");
        command.arg("30").process_group(0);
        let mut child = command.spawn().unwrap();
        let process = state.sessions.register(id, child.id()).unwrap();
        let started = tokio::time::Instant::now();

        shutdown_daemon_resources(&state).await;

        assert_eq!(tokio::time::Instant::now() - started, DAEMON_SHUTDOWN_GRACE);
        let _ = child.wait();
        state.sessions.finish(id, &process);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_returns_as_soon_as_the_last_session_is_reaped() {
        let state = DaemonServerState::default();
        let id = WireSessionId::new(2).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sleep");
        command.arg("30").process_group(0);
        let mut child = command.spawn().unwrap();
        let process = state.sessions.register(id, child.id()).unwrap();
        let started = tokio::time::Instant::now();
        let mut shutdown = Box::pin(shutdown_daemon_resources(&state));

        tokio::select! {
            biased;
            () = &mut shutdown => panic!("shutdown completed while a session was registered"),
            () = tokio::task::yield_now() => {}
        }
        state.sessions.finish(id, &process);
        shutdown.await;

        assert_eq!(tokio::time::Instant::now(), started);
        let _ = child.kill();
        let _ = child.wait();
    }
}
