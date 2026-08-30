//! Typed systemd-native OCI acquisition shared by direct and elevated modes.

use crate::adapters::process::SpawnedProcess;
use crate::domain::machine::MachineName;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::OciReference;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

const IMPORT_READ_ONLY: u64 = 1 << 1;

#[proxy(
    interface = "org.freedesktop.import1.Manager",
    default_service = "org.freedesktop.import1",
    default_path = "/org/freedesktop/import1"
)]
trait ImportManager {
    #[zbus(allow_interactive_auth)]
    fn pull_oci(
        &self,
        remote: &str,
        local: &str,
        image_class: &str,
        flags: u64,
    ) -> zbus::Result<(u32, OwnedObjectPath)>;
    #[zbus(allow_interactive_auth)]
    fn cancel_transfer(&self, transfer_id: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn transfer_removed(
        &self,
        transfer_id: u32,
        path: OwnedObjectPath,
        result: String,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.import1.Transfer",
    default_service = "org.freedesktop.import1"
)]
trait ImportTransfer {
    #[zbus(signal)]
    fn log_message(&self, priority: u32, line: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn progress_update(&self, progress: f64) -> zbus::Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OciPullRequest {
    pub(crate) reference: OciReference,
    pub(crate) machine: MachineName,
    pub(crate) read_only: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OciPullStore;

impl OciPullStore {
    pub fn new() -> Self {
        Self
    }

    pub(crate) async fn spawn(&self, request: OciPullRequest) -> Result<SpawnedProcess> {
        spawn_local_pull(request)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OciTransferCancellation {
    requested: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl OciTransferCancellation {
    pub(crate) fn request(&self) {
        if !self.requested.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OciTransferOutcome {
    Done,
    Cancelled,
    Failed(String),
}

impl OciTransferOutcome {
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::Done => 0,
            Self::Cancelled => 130,
            Self::Failed(_) => 1,
        }
    }
}

fn pull_flags(request: &OciPullRequest) -> u64 {
    if request.read_only {
        IMPORT_READ_ONLY
    } else {
        0
    }
}

fn spawn_local_pull(request: OciPullRequest) -> Result<SpawnedProcess> {
    let (writer, reader) = tokio::net::unix::pipe::pipe()
        .map_err(|error| NspawnError::Io(PathBuf::from("OCI transfer pipe"), error))?;
    let cancellation = OciTransferCancellation::default();
    let task_cancellation = cancellation.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = run_oci_transfer(request, writer, task_cancellation).await;
        let wait_result = result
            .map(|outcome| std::process::ExitStatus::from_raw(outcome.exit_code() << 8))
            .map_err(|error| std::io::Error::other(error.to_string()));
        let _ = done_tx.send(wait_result);
    });

    let signal_cancellation = cancellation.clone();
    Ok(SpawnedProcess::new_cancellable(
        Box::new(reader),
        async move {
            done_rx.await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "OCI transfer task ended without a result",
                )
            })?
        },
        move |_| {
            let cancellation = signal_cancellation.clone();
            Box::pin(async move {
                cancellation.request();
                Ok(())
            })
        },
    )
    .with_completion_wins_cancellation())
}

pub(crate) async fn run_oci_transfer<W>(
    request: OciPullRequest,
    mut output: W,
    cancellation: OciTransferCancellation,
) -> Result<OciTransferOutcome>
where
    W: AsyncWrite + Unpin,
{
    if cancellation.is_requested() {
        return Ok(OciTransferOutcome::Cancelled);
    }

    let connection = zbus::Connection::system()
        .await
        .map_err(NspawnError::Dbus)?;
    let manager = ImportManagerProxy::new(&connection)
        .await
        .map_err(NspawnError::Dbus)?;
    // Install the completion match before PullOci so a very small image cannot
    // finish between the method reply and signal subscription.
    let mut removed = manager
        .receive_transfer_removed()
        .await
        .map_err(NspawnError::Dbus)?;
    let (transfer_id, transfer_path) = manager
        .pull_oci(
            request.reference.as_str(),
            request.machine.as_str(),
            "machine",
            pull_flags(&request),
        )
        .await
        .map_err(NspawnError::Dbus)?;

    write_transfer_line(
        &mut output,
        format!("Enqueued systemd-importd transfer {transfer_id}."),
    )
    .await;

    let transfer = ImportTransferProxy::builder(&connection)
        .path(transfer_path.clone())
        .map_err(NspawnError::Dbus)?
        .build()
        .await
        .map_err(NspawnError::Dbus)?;
    let mut messages = Some(
        transfer
            .receive_log_message()
            .await
            .map_err(NspawnError::Dbus)?,
    );
    let mut progress = Some(
        transfer
            .receive_progress_update()
            .await
            .map_err(NspawnError::Dbus)?,
    );
    let mut cancel_sent = false;
    let mut last_progress_bucket = None;

    loop {
        tokio::select! {
            _ = cancellation.cancelled(), if !cancel_sent => {
                cancel_sent = true;
                write_transfer_line(
                    &mut output,
                    format!("Cancelling systemd-importd transfer {transfer_id}..."),
                ).await;
                if let Err(error) = manager.cancel_transfer(transfer_id).await {
                    // The transfer may have completed between the local
                    // cancellation request and CancelTransfer. Keep waiting
                    // for its authoritative TransferRemoved result; the outer
                    // wait timeout will fail closed if no result arrives.
                    log::warn!(
                        "CancelTransfer({transfer_id}) failed while awaiting final transfer state: {error}"
                    );
                    write_transfer_line(
                        &mut output,
                        format!(
                            "WARNING: cancellation request failed ({error}); awaiting authoritative transfer result."
                        ),
                    ).await;
                }
            }
            event = removed.next() => {
                let event = event.ok_or_else(|| {
                    NspawnError::Runtime(format!(
                        "systemd-importd completion stream closed while transfer {transfer_id} was active"
                    ))
                })?;
                let args = event.args().map_err(NspawnError::Dbus)?;
                if *args.transfer_id() != transfer_id || args.path() != &transfer_path {
                    continue;
                }
                let result = args.result();
                write_transfer_line(
                    &mut output,
                    format!("systemd-importd transfer {transfer_id} finished: {result}"),
                ).await;
                return Ok(match result.as_str() {
                    "done" => OciTransferOutcome::Done,
                    "canceled" => OciTransferOutcome::Cancelled,
                    other => OciTransferOutcome::Failed(other.to_string()),
                });
            }
            event = async {
                match messages.as_mut() {
                    Some(messages) => messages.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Some(event) => {
                        let args = event.args().map_err(NspawnError::Dbus)?;
                        write_transfer_line(&mut output, args.line()).await;
                    }
                    None => messages = None,
                }
            }
            event = async {
                match progress.as_mut() {
                    Some(progress) => progress.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Some(event) => {
                        let args = event.args().map_err(NspawnError::Dbus)?;
                        let bucket = ((*args.progress() * 100.0).clamp(0.0, 100.0) as u8) / 5 * 5;
                        if last_progress_bucket != Some(bucket) {
                            last_progress_bucket = Some(bucket);
                            write_transfer_line(
                                &mut output,
                                format!("OCI transfer progress: {bucket}%"),
                            ).await;
                        }
                    }
                    None => progress = None,
                }
            }
        }
    }
}

async fn write_transfer_line(output: &mut (impl AsyncWrite + Unpin), line: impl AsRef<str>) {
    let write = async {
        output.write_all(line.as_ref().as_bytes()).await?;
        output.write_all(b"\n").await
    }
    .await;
    if let Err(error) = write {
        log::debug!("OCI transfer output receiver closed: {error}");
    }
}

/// Probe the actual importctl verb instead of trusting a parsed version number.
pub fn ensure_pull_oci_available() -> Result<()> {
    let output = crate::adapters::process::new_sync_command("importctl")
        .arg("--help")
        .output()
        .map_err(|error| NspawnError::Io(PathBuf::from("importctl --help"), error))?;
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success()
        && help
            .lines()
            .any(|line| line.trim_start().starts_with("pull-oci "))
    {
        Ok(())
    } else {
        Err(NspawnError::Validation(
            "OCI applications require systemd 260 or newer with importctl pull-oci".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(read_only: bool) -> OciPullRequest {
        OciPullRequest {
            reference: OciReference::new("docker.io/library/nginx:latest").unwrap(),
            machine: MachineName::new("web-app").unwrap(),
            read_only,
        }
    }

    #[test]
    fn writable_pull_uses_no_import_flags() {
        assert_eq!(pull_flags(&request(false)), 0);
    }

    #[test]
    fn read_only_maps_to_systemd_import_flag() {
        assert_eq!(pull_flags(&request(true)), IMPORT_READ_ONLY);
    }

    #[tokio::test]
    async fn transfer_cancellation_notifies_waiters() {
        let cancellation = OciTransferCancellation::default();
        let waiter = cancellation.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });

        cancellation.request();
        task.await.unwrap();
        assert!(cancellation.is_requested());
    }

    #[tokio::test]
    async fn cancellation_before_enqueue_never_opens_a_transfer() {
        let cancellation = OciTransferCancellation::default();
        cancellation.request();

        let outcome = run_oci_transfer(request(false), tokio::io::sink(), cancellation)
            .await
            .unwrap();

        assert_eq!(outcome, OciTransferOutcome::Cancelled);
    }

    #[test]
    fn transfer_outcomes_map_to_process_compatible_status_codes() {
        assert_eq!(OciTransferOutcome::Done.exit_code(), 0);
        assert_ne!(OciTransferOutcome::Cancelled.exit_code(), 0);
        assert_ne!(OciTransferOutcome::Failed("network".into()).exit_code(), 0);
    }

    #[test]
    fn request_deserialization_rejects_arbitrary_fields() {
        let json = r#"{
            "reference":"docker.io/library/nginx:latest",
            "machine":"web-app",
            "read_only":false,
            "program":"sh"
        }"#;
        assert!(serde_json::from_str::<OciPullRequest>(json).is_err());
    }
}
