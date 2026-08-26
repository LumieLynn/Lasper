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
    stream: UnixStream,
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
            run_session_worker(stream, move |stream| {
                sessions::server::spawn_journal(stream, params, server_state)
            })
            .await;
        }
        FdOperation::Terminal(params) => {
            run_session_worker(stream, move |stream| {
                sessions::server::spawn_terminal(stream, params, server_state)
            })
            .await;
        }
        FdOperation::SubmitDeployment(params) => {
            jobs::server::submit(&stream, *params, server_state, trusted_state_root).await;
        }
    }
}

async fn run_session_worker<F>(mut stream: UnixStream, operation: F)
where
    F: FnOnce(&mut UnixStream) + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || {
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
        operation(&mut stream);
        std::io::Result::Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::error!("Daemon session FD worker failed: {error}"),
        Err(error) => log::error!("Daemon session FD worker panicked: {error}"),
    }
}
