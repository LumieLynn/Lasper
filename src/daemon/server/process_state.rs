//! Daemon-owned child process outcomes, cancellation, and shutdown cleanup.

use super::DaemonServerState;

const DAEMON_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(750);
const DEPLOYMENT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

pub(crate) async fn shutdown_daemon_resources(server_state: &DaemonServerState) {
    if !server_state
        .deployments
        .cancel_all_and_wait(DEPLOYMENT_SHUTDOWN_GRACE)
        .await
    {
        log::error!(
            "Daemon: provisioning jobs did not reach a terminal state before shutdown; child parent-death guards remain armed"
        );
    }

    let processes = server_state.session_processes();
    let mut needs_session_grace = false;
    for process in &processes {
        match server_state.signal_session_process(process, libc::SIGTERM) {
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

    if server_state
        .wait_for_sessions_to_finish(DAEMON_SHUTDOWN_GRACE)
        .await
    {
        return;
    }

    for process in server_state.session_processes() {
        if let Err(error) = server_state.signal_session_process(&process, libc::SIGKILL) {
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
        let process = state.register(id, child.id()).unwrap();
        let started = tokio::time::Instant::now();

        shutdown_daemon_resources(&state).await;

        assert_eq!(tokio::time::Instant::now() - started, DAEMON_SHUTDOWN_GRACE);
        let _ = child.wait();
        state.finish(id, &process);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_returns_as_soon_as_the_last_session_is_reaped() {
        let state = DaemonServerState::default();
        let id = WireSessionId::new(2).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sleep");
        command.arg("30").process_group(0);
        let mut child = command.spawn().unwrap();
        let process = state.register(id, child.id()).unwrap();
        let started = tokio::time::Instant::now();
        let mut shutdown = Box::pin(shutdown_daemon_resources(&state));

        tokio::select! {
            biased;
            () = &mut shutdown => panic!("shutdown completed while a session was registered"),
            () = tokio::task::yield_now() => {}
        }
        state.finish(id, &process);
        shutdown.await;

        assert_eq!(tokio::time::Instant::now(), started);
        let _ = child.kill();
        let _ = child.wait();
    }
}
