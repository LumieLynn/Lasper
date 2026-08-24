//! Shared daemon-owned state used by the server, jobs, and sessions.

use crate::daemon::jobs::server::DeploymentRegistry;
use crate::daemon::protocol::session::WireSessionId;
use std::collections::HashMap;
use std::sync::Arc;

/// Runtime state that is shared by authenticated daemon handlers.
///
/// The fields are crate-private because the daemon's family modules are still
/// being migrated toward narrower capability views. Keeping the aggregate in
/// `server::state` makes that remaining coupling explicit instead of hiding it
/// inside the session process implementation.
#[derive(Default)]
pub(crate) struct DaemonServerState {
    sessions: parking_lot::Mutex<HashMap<WireSessionId, u32>>,
    pub(crate) deployments: DeploymentRegistry,
    pub(crate) operations: Arc<crate::application::OperationRegistry>,
}

impl DaemonServerState {
    pub(crate) fn register(&self, id: WireSessionId, pid: u32) -> std::io::Result<()> {
        let mut sessions = self.sessions.lock();
        if sessions.contains_key(&id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session {} is already open", id.get()),
            ));
        }
        sessions.insert(id, pid);
        Ok(())
    }

    pub(crate) fn finish(&self, id: WireSessionId, pid: u32) {
        let mut sessions = self.sessions.lock();
        if sessions.get(&id) == Some(&pid) {
            sessions.remove(&id);
        }
    }

    pub(crate) fn close_and_escalate(self: &Arc<Self>, id: WireSessionId) -> std::io::Result<()> {
        let Some(pid) = self.sessions.lock().get(&id).copied() else {
            return Ok(());
        };
        if let Err(error) = crate::adapters::process::signal_process_group(pid, libc::SIGTERM) {
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            if state.sessions.lock().get(&id).copied() == Some(pid) {
                if let Err(error) =
                    crate::adapters::process::signal_process_group(pid, libc::SIGKILL)
                {
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        log::warn!(
                            "failed to force-close session {} process group {}: {error}",
                            id.get(),
                            pid
                        );
                    }
                }
            }
        });
        Ok(())
    }

    pub(crate) fn pids(&self) -> Vec<u32> {
        self.sessions.lock().values().copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions.lock().len()
    }
}
