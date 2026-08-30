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
    for process in &processes {
        if let Err(error) = server_state.signal_session_process(process, libc::SIGTERM) {
            log::warn!(
                "Daemon: failed to terminate child process group {}: {error}",
                process.pid()
            );
        }
    }

    tokio::time::sleep(DAEMON_SHUTDOWN_GRACE).await;

    for process in server_state.session_processes() {
        if let Err(error) = server_state.signal_session_process(&process, libc::SIGKILL) {
            log::warn!(
                "Daemon: failed to kill child process group {}: {error}",
                process.pid()
            );
        }
    }
}
