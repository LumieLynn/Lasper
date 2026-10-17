//! Shared ownership of journal and PTY children from FD handoff through reap.

use crate::daemon::sessions::lifecycle;
use crate::daemon::sessions::registry::{SessionProcess, SessionRegistry};
use crate::domain::machine::MachineName;
use crate::domain::session::TerminalAttachmentKind;
use crate::ipc::protocol::session::{WireSessionId, WireSessionLifecycle};
use anyhow::Context;
use sendfd::SendWithFd;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;

pub(super) enum SessionChild {
    Journal(std::process::Child),
    Terminal {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        name: MachineName,
        kind: TerminalAttachmentKind,
    },
}

impl SessionChild {
    fn pid(&self) -> std::io::Result<u32> {
        match self {
            Self::Journal(child) => Ok(child.id()),
            Self::Terminal { child, .. } => child
                .process_id()
                .ok_or_else(|| std::io::Error::other("terminal child has no process id")),
        }
    }

    fn terminate(&mut self) {
        let result = match self {
            Self::Journal(child) => {
                crate::adapters::process::signal_process_group(child.id(), libc::SIGKILL)
            }
            Self::Terminal { child, .. } => match child.process_id() {
                Some(pid) => crate::adapters::process::signal_process_group(pid, libc::SIGKILL),
                // Native Linux PTYs report a PID. If a child implementation
                // cannot, failed startup can only use its own cancellation.
                None => child.kill(),
            },
        };
        if let Err(error) = result {
            if error.raw_os_error() != Some(libc::ESRCH) {
                log::warn!("Daemon: failed to terminate session child: {error}");
            }
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<WireSessionLifecycle>> {
        match self {
            Self::Journal(child) => {
                Ok(child
                    .try_wait()?
                    .map(|status| WireSessionLifecycle::Exited {
                        success: status.success(),
                        code: status.code(),
                    }))
            }
            Self::Terminal { child, name, kind } => Ok(child.try_wait()?.map(|status| {
                if status.success() {
                    log::info!("Terminal attachment for {name} ({kind:?}) exited normally");
                } else {
                    log::warn!(
                        "Terminal attachment for {name} ({kind:?}) exited with code {} signal {:?}",
                        status.exit_code(),
                        status.signal()
                    );
                }
                WireSessionLifecycle::Exited {
                    success: status.success(),
                    code: i32::try_from(status.exit_code()).ok(),
                }
            })),
        }
    }

    async fn reap(
        &mut self,
        mut readiness: Option<&AsyncFd<SessionProcess>>,
    ) -> WireSessionLifecycle {
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let message = format!("wait for session child: {error}");
                    log::warn!("Daemon: {message}");
                    return WireSessionLifecycle::Failed { message };
                }
            }
            if let Some(pidfd) = readiness {
                match pidfd.readable().await {
                    Ok(mut ready) => ready.clear_ready(),
                    Err(error) => {
                        log::warn!("Daemon: session pidfd monitor failed: {error}");
                        self.terminate();
                        readiness = None;
                    }
                }
            } else {
                // Without a usable pidfd, retain the terminated child until
                // waitpid reaps it. Live sessions do not use this polling path.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
}

/// Takes ownership even when registration or FD delivery fails. Startup runs
/// in a short blocking worker; the child is then owned by an async reaper.
pub(super) fn deliver(
    mut child: SessionChild,
    sessions: Arc<SessionRegistry>,
    id: WireSessionId,
    stream: &UnixStream,
    response: &[u8],
    output: RawFd,
) -> anyhow::Result<()> {
    let setup = (|| {
        let (reader, writer) = lifecycle::channel()?;
        let process = sessions.register(id, child.pid()?)?;
        std::io::Result::Ok((reader, writer, process))
    })();
    let (reader, writer, process) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            child.terminate();
            tokio::spawn(async move { child.reap(None).await });
            return Err(error).context("prepare session process monitor");
        }
    };

    let (readiness, delivered) = match AsyncFd::new(process.clone()) {
        Ok(readiness) => (
            Some(readiness),
            stream
                .send_with_fd(response, &[output, reader.as_raw_fd()])
                .context("deliver session descriptors"),
        ),
        Err(error) => (None, Err(error).context("register session pidfd readiness")),
    };
    if delivered.is_err() {
        child.terminate();
    }
    tokio::spawn(async move {
        let status = child.reap(readiness.as_ref()).await;
        sessions.finish(id, &process);
        lifecycle::write(writer, status).await;
    });
    delivered?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendfd::RecvWithFd;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    struct Client {
        _output: OwnedFd,
        status: OwnedFd,
        pid: u32,
    }

    fn receive(stream: UnixStream, pid: u32) -> Client {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut message = [0; 64];
        let mut fds = [-1; 2];
        let (_, count) = stream.recv_with_fd(&mut message, &mut fds).unwrap();
        let mut owned = fds[..count]
            .iter()
            .map(|fd| unsafe { OwnedFd::from_raw_fd(*fd) })
            .collect::<Vec<_>>();
        assert_eq!(owned.len(), 2);
        let status = owned.pop().unwrap();
        Client {
            _output: owned.pop().unwrap(),
            status,
            pid,
        }
    }

    fn journal(sessions: Arc<SessionRegistry>, id: WireSessionId, script: &str) -> Client {
        let (server, client) = UnixStream::pair().unwrap();
        let mut child = crate::adapters::process::new_sync_command("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        deliver(
            SessionChild::Journal(child),
            sessions,
            id,
            &server,
            b"ok",
            stdout.as_raw_fd(),
        )
        .unwrap();
        receive(client, pid)
    }

    fn terminal(sessions: Arc<SessionRegistry>, id: WireSessionId, script: &str) -> Client {
        let (server, client) = UnixStream::pair().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("sh");
        command.args(["-c", script]);
        let child = pair.slave.spawn_command(command).unwrap();
        let pid = child.process_id().unwrap();
        drop(pair.slave);
        deliver(
            SessionChild::Terminal {
                child,
                name: MachineName::try_from("test").unwrap(),
                kind: TerminalAttachmentKind::Login,
            },
            sessions,
            id,
            &server,
            b"ok",
            pair.master.as_raw_fd().unwrap(),
        )
        .unwrap();
        receive(client, pid)
    }

    async fn status(reader: OwnedFd) -> WireSessionLifecycle {
        let mut reader = tokio::net::unix::pipe::Receiver::from_owned_fd(reader).unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn assert_reaped(pid: u32) {
        let result =
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(result, -1, "session owner must reap the child");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn live_journal_and_pty_sessions_do_not_exhaust_the_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let sessions = Arc::new(SessionRegistry::default());
            let journal_id = WireSessionId::new(1).unwrap();
            let terminal_id = WireSessionId::new(2).unwrap();
            let journal = journal(sessions.clone(), journal_id, "exec sleep 10");
            let terminal = terminal(sessions.clone(), terminal_id, "exec sleep 10");
            tokio::task::yield_now().await;

            let progress = tokio::time::timeout(Duration::from_secs(1), async {
                let value = tokio::task::spawn_blocking(|| 42).await.unwrap();
                let stat = tokio::fs::read("/proc/self/statm").await.unwrap();
                (value, stat)
            })
            .await;

            sessions.close_and_escalate(journal_id).unwrap();
            sessions.close_and_escalate(terminal_id).unwrap();
            let reaped = sessions.wait_for_empty(Duration::from_secs(2)).await;
            assert!(reaped);
            assert!(matches!(
                status(journal.status).await,
                WireSessionLifecycle::Exited { success: false, .. }
            ));
            assert!(matches!(
                status(terminal.status).await,
                WireSessionLifecycle::Exited { success: false, .. }
            ));
            assert_reaped(journal.pid);
            assert_reaped(terminal.pid);
            let (value, stat) = progress.expect("long sessions occupied the only blocking worker");
            assert_eq!(value, 42);
            assert!(!stat.is_empty());
        });
    }

    #[tokio::test]
    async fn natural_exit_delivers_status_and_reaps_both_child_types() {
        let sessions = Arc::new(SessionRegistry::default());
        let journal = journal(sessions.clone(), WireSessionId::new(1).unwrap(), "exit 7");
        let terminal = terminal(sessions.clone(), WireSessionId::new(2).unwrap(), "exit 7");
        for client in [journal, terminal] {
            assert!(matches!(
                status(client.status).await,
                WireSessionLifecycle::Exited {
                    success: false,
                    code: Some(7)
                }
            ));
            assert_reaped(client.pid);
        }
        assert_eq!(sessions.len(), 0);
    }

    #[tokio::test]
    async fn failed_handoff_terminates_and_reaps_the_registered_child() {
        let sessions = Arc::new(SessionRegistry::default());
        let (server, client) = UnixStream::pair().unwrap();
        drop(client);
        let mut child = crate::adapters::process::new_sync_command("sleep")
            .arg("10")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let result = deliver(
            SessionChild::Journal(child),
            sessions.clone(),
            WireSessionId::new(1).unwrap(),
            &server,
            b"ok",
            stdout.as_raw_fd(),
        );
        let reaped = sessions.wait_for_empty(Duration::from_secs(2)).await;

        assert!(result.is_err());
        assert!(reaped);
        assert_reaped(pid);
    }
}
