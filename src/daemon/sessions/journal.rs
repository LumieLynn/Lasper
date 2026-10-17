//! Journal process startup and delivery of its output stream.

use crate::daemon::sessions::handoff;
use crate::daemon::sessions::process::{self, SessionChild};
use crate::daemon::sessions::SessionRegistry;
use crate::ipc::protocol::session::SpawnJournalctlParams;
use anyhow::Context;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::sync::Arc;

pub(crate) async fn open(
    stream: UnixStream,
    params: SpawnJournalctlParams,
    sessions: Arc<SessionRegistry>,
) {
    handoff::respond(stream, move |stream| {
        let SpawnJournalctlParams { session_id, name } = params;
        let mut child = crate::adapters::process::new_sync_command("journalctl")
            .args([
                "-M",
                name.as_str(),
                "-n",
                "1000",
                "-f",
                "--no-pager",
                "--output=short",
            ])
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .context("spawn journalctl")?;
        let stdout = child.stdout.take().expect("journalctl stdout is piped");
        process::deliver(
            SessionChild::Journal(child),
            sessions,
            session_id,
            stream,
            b"ok",
            stdout.as_raw_fd(),
        )
    })
    .await;
}
