//! FD-channel operation dispatch.
//!
//! Authentication and socket setup remain in `server`; this module only
//! routes an authenticated typed operation to the subsystem that owns it.

use super::super::jobs;
use super::super::protocol::FdOperation;
use super::super::sessions;
use super::DaemonServerState;
use crate::adapters::trusted_state::TrustedStateRoot;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

pub(super) async fn handle(
    stream: &mut UnixStream,
    operation: FdOperation,
    server_state: Arc<DaemonServerState>,
    trusted_state_root: TrustedStateRoot,
) {
    log::trace!(
        "Daemon FD operation {} ({})",
        operation.wire_name(),
        operation.family().as_str()
    );

    match operation {
        FdOperation::Journalctl(params) => {
            sessions::server::spawn_journal(stream, params, server_state);
        }
        FdOperation::Terminal(params) => {
            sessions::server::spawn_terminal(stream, params, server_state);
        }
        FdOperation::SubmitDeployment(params) => {
            jobs::server::submit(stream, *params, server_state, trusted_state_root).await;
        }
    }
}
