use crate::adapters::process::SpawnedProcess;
use crate::application::provisioning::{
    DeploymentCancellation, DeploymentEvent as DeployLogEvent, DeploymentProgress as DeployProgress,
};
use crate::nspawn::errors::{NspawnError, Result};
use tokio::sync::mpsc::Sender;

pub(crate) async fn send_deploy_log(logs: &Sender<DeployLogEvent>, message: impl Into<String>) {
    let message = message.into();
    log::info!("[DEPLOY] {}", message);
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

pub(crate) async fn send_deploy_stream_log(
    logs: &Sender<DeployLogEvent>,
    message: impl Into<String>,
) {
    let message = message.into();
    if is_high_signal_deploy_stream(&message) {
        log::warn!("[DEPLOY stream] {}", message);
    } else {
        // Deployment output must survive the wizard and be available in the
        // normal per-run log without requiring RUST_LOG=debug.
        log::info!("[DEPLOY stream] {}", message);
    }
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

pub(crate) fn is_high_signal_deploy_stream(message: &str) -> bool {
    let message = message.trim_start().to_ascii_lowercase();
    ["w:", "e:", "warning:", "error:", "fatal:"]
        .iter()
        .any(|prefix| message.starts_with(prefix))
        || [
            "permission denied",
            "operation not permitted",
            "failed",
            "failure",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

pub(crate) async fn send_deploy_progress(
    logs: &Sender<DeployLogEvent>,
    label: impl Into<String>,
    permille: u16,
) {
    let progress = DeployProgress::new(label, permille);
    log::trace!(
        "[DEPLOY progress] {}: {}.{:01}%",
        progress.label,
        progress.permille / 10,
        progress.permille % 10
    );
    let _ = logs.send(DeployLogEvent::Progress(progress)).await;
}

pub(crate) async fn stream_deploy_command(
    mut spawned: SpawnedProcess,
    logs: &Sender<DeployLogEvent>,
    cancellation: &DeploymentCancellation,
    label: &str,
) -> Result<std::process::ExitStatus> {
    use tokio::io::AsyncBufReadExt;

    let mut cancelled = false;
    let mut stream_error = None;
    {
        let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    cancelled = true;
                    break;
                }
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => send_deploy_stream_log(logs, line).await,
                        Ok(None) => break,
                        Err(error) => {
                            stream_error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
    }

    if cancelled || cancellation.is_requested() {
        send_deploy_log(logs, format!("Stopping {label}...")).await;
        let completion_wins = spawned.completion_wins_cancellation();
        let status = spawned
            .terminate_and_wait()
            .await
            .map_err(|error| process_state_unknown(label, error))?;
        if completion_wins && status.success() {
            return Ok(status);
        }
        return Err(NspawnError::DeploymentCancelled);
    }

    if let Some(error) = stream_error {
        send_deploy_log(
            logs,
            format!("Stopping {label} after its output stream failed..."),
        )
        .await;
        spawned
            .terminate_and_wait()
            .await
            .map_err(|wait_error| process_state_unknown(label, wait_error))?;
        return Err(NspawnError::Io(std::path::PathBuf::from(label), error));
    }

    spawned
        .wait()
        .await
        .map_err(|error| process_state_unknown(label, error))
}

pub(crate) fn process_state_unknown(label: &str, error: std::io::Error) -> NspawnError {
    NspawnError::DeploymentProcessStateUnknown(format!(
        "could not confirm that {label} exited: {error}"
    ))
}
