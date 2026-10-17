//! Identity and cancellation of daemon-owned session processes.

use crate::adapters::process::signal_process_group;
use crate::ipc::protocol::session::WireSessionId;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Ownership record for a session child.
///
/// The pidfd pins the leader's kernel identity for liveness checks. Group
/// termination still uses the numeric process group and is revalidated just
/// before each signal; pidfd does not make that separate group operation
/// atomic. The completion flag prevents delayed escalation from racing a
/// normal reap.
#[derive(Clone, Debug)]
pub(crate) struct SessionProcess {
    pid: u32,
    pidfd: Arc<OwnedFd>,
    completed: Arc<AtomicBool>,
}

impl SessionProcess {
    fn capture(pid: u32) -> std::io::Result<Self> {
        Ok(Self {
            pid,
            pidfd: Arc::new(crate::adapters::process::open_pidfd(pid)?),
            completed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    fn same_process(&self, other: &SessionProcess) -> bool {
        self.pid == other.pid && Arc::ptr_eq(&self.pidfd, &other.pidfd)
    }

    fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }
}

impl AsRawFd for SessionProcess {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.pidfd.as_raw_fd()
    }
}

/// Registered children remain owned by their reapers until exit. RPC close
/// requests and daemon shutdown use these handles to signal the same processes.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    sessions: parking_lot::Mutex<HashMap<WireSessionId, SessionProcess>>,
    session_changes: tokio::sync::Notify,
}

impl SessionRegistry {
    pub(crate) fn register(&self, id: WireSessionId, pid: u32) -> std::io::Result<SessionProcess> {
        if self.sessions.lock().contains_key(&id) {
            return Err(Self::session_already_open(id));
        }
        let process = SessionProcess::capture(pid)?;
        let mut sessions = self.sessions.lock();
        if sessions.contains_key(&id) {
            return Err(Self::session_already_open(id));
        }
        sessions.insert(id, process.clone());
        Ok(process)
    }

    pub(crate) fn finish(&self, id: WireSessionId, process: &SessionProcess) {
        let removed = {
            let mut sessions = self.sessions.lock();
            if sessions
                .get(&id)
                .is_some_and(|current| current.same_process(process))
            {
                if let Some(process) = sessions.get(&id) {
                    process.mark_completed();
                }
                sessions.remove(&id);
                true
            } else {
                false
            }
        };
        if removed {
            self.session_changes.notify_waiters();
        }
    }

    pub(crate) fn close_and_escalate(self: &Arc<Self>, id: WireSessionId) -> std::io::Result<()> {
        let Some(process) = self.sessions.lock().get(&id).cloned() else {
            return Ok(());
        };
        if !self.revalidate_process(&process)? {
            self.remove_if_process(id, &process);
            return Ok(());
        }
        if let Err(error) = signal_process_group(process.pid(), libc::SIGTERM) {
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            let still_owned = state
                .sessions
                .lock()
                .get(&id)
                .is_some_and(|current| current.same_process(&process));
            if !still_owned || process.is_completed() {
                return;
            }
            match state.revalidate_process(&process) {
                Ok(false) => {
                    state.remove_if_process(id, &process);
                }
                Ok(true) => {
                    if let Err(error) = signal_process_group(process.pid(), libc::SIGKILL) {
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            log::warn!(
                                "failed to force-close session {} process group {}: {error}",
                                id.get(),
                                process.pid()
                            );
                        }
                    }
                }
                Err(error) => log::warn!(
                    "failed to revalidate session {} process {} before SIGKILL: {error}",
                    id.get(),
                    process.pid()
                ),
            }
        });
        Ok(())
    }

    fn session_already_open(id: WireSessionId) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("session {} is already open", id.get()),
        )
    }

    /// Return owned process handles for daemon shutdown. Callers must still
    /// revalidate each handle immediately before signaling it.
    pub(crate) fn processes(&self) -> Vec<SessionProcess> {
        self.sessions.lock().values().cloned().collect()
    }

    /// Wait until every registered session has been reaped, bounded by the
    /// caller's shutdown deadline. Register the notification waiter before
    /// inspecting the map so a concurrent final reap cannot be missed.
    pub(crate) async fn wait_for_empty(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.session_changes.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.sessions.lock().is_empty() {
                return true;
            }
            if tokio::time::timeout_at(deadline, &mut changed)
                .await
                .is_err()
            {
                return self.sessions.lock().is_empty();
            }
        }
    }

    pub(crate) fn signal(&self, process: &SessionProcess, signal: i32) -> std::io::Result<bool> {
        if process.is_completed() || !self.revalidate_process(process)? {
            return Ok(false);
        }
        match signal_process_group(process.pid(), signal) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn revalidate_process(&self, process: &SessionProcess) -> std::io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: process.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if result == 0 {
            return Ok(true);
        }
        Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0)
    }

    fn remove_if_process(&self, id: WireSessionId, process: &SessionProcess) {
        let removed = {
            let mut sessions = self.sessions.lock();
            if sessions
                .get(&id)
                .is_some_and(|current| current.same_process(process))
            {
                sessions.remove(&id);
                true
            } else {
                false
            }
        };
        if removed {
            self.session_changes.notify_waiters();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn duplicate_ids_and_cleanup_cannot_replace_another_process() {
        let state = SessionRegistry::default();
        let id = WireSessionId::new(1).unwrap();
        let mut first = crate::adapters::process::new_sync_command("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let first_process = state.register(id, first.id()).unwrap();
        let mut second = crate::adapters::process::new_sync_command("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let second_id = WireSessionId::new(2).unwrap();
        let second_process = state.register(second_id, second.id()).unwrap();
        let duplicate_error = state.register(id, u32::MAX).unwrap_err();
        state.finish(id, &second_process);
        let remaining_after_wrong_process = state.len();
        state.finish(id, &first_process);
        state.finish(second_id, &second_process);
        let _ = first.kill();
        let _ = first.wait();
        let _ = second.kill();
        let _ = second.wait();

        assert_eq!(duplicate_error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(remaining_after_wrong_process, 2);
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn an_already_reaped_process_is_removed_without_signaling_a_reused_pid() {
        let state = Arc::new(SessionRegistry::default());
        let id = WireSessionId::new(900).unwrap();
        let mut child = crate::adapters::process::new_sync_command("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _process = state.register(id, child.id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        state.close_and_escalate(id).unwrap();
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn escalation_removes_a_session_after_term_reaps_the_leader() {
        let state = Arc::new(SessionRegistry::default());
        let id = WireSessionId::new(901).unwrap();
        let mut command = crate::adapters::process::new_sync_command("sh");
        command.args(["-c", "exec sleep 30"]).process_group(0);
        let mut child = command.spawn().unwrap();
        state.register(id, child.id()).unwrap();

        state.close_and_escalate(id).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());

        tokio::time::sleep(std::time::Duration::from_millis(850)).await;
        assert_eq!(state.len(), 0);
    }
}
