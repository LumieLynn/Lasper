//! FD-channel operation dispatch.
//!
//! Authentication and socket setup remain in `server`; this module only
//! routes an authenticated typed operation to the subsystem that owns it.

use super::DaemonServerState;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::daemon::jobs;
use crate::daemon::sessions;
use crate::ipc::protocol::FdOperation;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

pub(super) async fn handle(
    stream: UnixStream,
    operation: FdOperation,
    server_state: Arc<DaemonServerState>,
    trusted_state_root: TrustedStateRoot,
    machine: crate::adapters::session::MachineSessionTransport,
    invoking_uid: u32,
) {
    let Some(_active_request) = server_state.shutdown.admit() else {
        return;
    };
    log::trace!(
        "Daemon FD operation {} ({})",
        operation.wire_name(),
        operation.family().as_str()
    );

    match operation {
        FdOperation::Journalctl(params) => {
            sessions::journal::open(stream, params, server_state.sessions.clone()).await;
        }
        FdOperation::Terminal(params) => {
            sessions::terminal::open(
                stream,
                params,
                server_state.sessions.clone(),
                machine,
                invoking_uid,
            )
            .await;
        }
        FdOperation::SubmitDeployment(params) => {
            jobs::server::submit(&stream, *params, server_state, trusted_state_root).await;
        }
    }
}
