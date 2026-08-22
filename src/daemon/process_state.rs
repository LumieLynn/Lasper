//! Daemon-owned child process outcomes, cancellation, and shutdown cleanup.

use super::session_server::DaemonServerState;
use crate::adapters::provisioning::engine::oci_operation::OciTransferCancellation;

const DAEMON_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

pub(super) static SPAWN_EXIT_CODES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, i32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub(super) static SPAWN_WAIT_ERRORS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub(super) static SPAWN_PIDS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, u32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub(super) static OCI_TRANSFER_CANCELLATIONS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, OciTransferCancellation>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

pub(super) async fn shutdown_daemon_resources(server_state: &DaemonServerState) {
    for cancellation in OCI_TRANSFER_CANCELLATIONS.lock().values() {
        cancellation.request();
    }

    let mut pids = std::collections::HashSet::new();
    pids.extend(SPAWN_PIDS.lock().values().copied());
    pids.extend(server_state.pids());
    for pid in &pids {
        if let Err(error) = crate::adapters::process::signal_process_group(*pid, libc::SIGTERM) {
            log::warn!("Daemon: failed to terminate child process group {pid}: {error}");
        }
    }

    tokio::time::sleep(DAEMON_SHUTDOWN_GRACE).await;

    let mut remaining = std::collections::HashSet::new();
    remaining.extend(SPAWN_PIDS.lock().values().copied());
    remaining.extend(server_state.pids());
    for pid in remaining {
        if let Err(error) = crate::adapters::process::signal_process_group(pid, libc::SIGKILL) {
            log::warn!("Daemon: failed to kill child process group {pid}: {error}");
        }
    }
}

pub(super) fn stop_unhanded_command(child: &mut std::process::Child, cmd_id: u64, label: &str) {
    let pid = child.id();
    if let Err(error) = crate::adapters::process::signal_process_group(pid, libc::SIGKILL) {
        log::error!("Daemon: failed to stop unhanded {label} command {cmd_id}: {error}");
    }
    if let Err(error) = child.wait() {
        log::error!("Daemon: failed to wait for unhanded {label} command {cmd_id}: {error}");
    }
    SPAWN_PIDS.lock().remove(&cmd_id);
}
