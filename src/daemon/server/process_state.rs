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

    let pids = server_state.pids();
    for pid in &pids {
        if let Err(error) = crate::adapters::process::signal_process_group(*pid, libc::SIGTERM) {
            log::warn!("Daemon: failed to terminate child process group {pid}: {error}");
        }
    }

    tokio::time::sleep(DAEMON_SHUTDOWN_GRACE).await;

    for pid in server_state.pids() {
        if let Err(error) = crate::adapters::process::signal_process_group(pid, libc::SIGKILL) {
            log::warn!("Daemon: failed to kill child process group {pid}: {error}");
        }
    }
}
