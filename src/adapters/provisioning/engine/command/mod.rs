//! Drain provider output and resolve process completion or cancellation before returning.

use crate::adapters::process::SpawnedProcess;
use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::application::provisioning::DeploymentCancellation;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_COMMAND_LINE_BYTES: usize = 16 * 1024;

pub(crate) async fn wait_for_deployment_command(
    mut spawned: SpawnedProcess,
    logs: &DeploymentDiagnostics,
    cancellation: &DeploymentCancellation,
    label: &str,
) -> OperationResult<std::process::ExitStatus> {
    let stream_result = tokio::select! {
        result = stream_command_output(&mut spawned.stdout, logs) => Some(result),
        _ = cancellation.cancelled() => None,
    };
    let cancelled = stream_result.is_none();
    let stream_error = stream_result.and_then(std::result::Result::err);

    if cancelled || cancellation.is_requested() {
        logs.message(format!("Stopping {label}..."));
        let completion_wins = spawned.completion_wins_cancellation();
        let status = spawned
            .terminate_and_wait()
            .await
            .map_err(|error| OperationFailure::process_exit_unconfirmed(label, error))?;
        if completion_wins && status.success() {
            return Ok(status);
        }
        return Err(OperationFailure::Cancelled);
    }

    if let Some(error) = stream_error {
        logs.message(format!(
            "Stopping {label} after its output stream failed..."
        ));
        spawned
            .terminate_and_wait()
            .await
            .map_err(|wait_error| OperationFailure::process_exit_unconfirmed(label, wait_error))?;
        return Err(OperationFailure::Io(std::path::PathBuf::from(label), error));
    }

    spawned
        .wait()
        .await
        .map_err(|error| OperationFailure::process_exit_unconfirmed(label, error))
}

/// Drain the complete stream while bounding each retained line, including non-UTF-8 output.
async fn stream_command_output(
    reader: &mut (impl AsyncRead + Unpin),
    diagnostics: &DeploymentDiagnostics,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    let mut line = Vec::new();
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                report_command_line(diagnostics, &line, truncated);
                line.clear();
                truncated = false;
            } else if line.len() < MAX_COMMAND_LINE_BYTES {
                line.push(*byte);
            } else {
                truncated = true;
            }
        }
    }
    if !line.is_empty() || truncated {
        report_command_line(diagnostics, &line, truncated);
    }
    Ok(())
}

fn report_command_line(diagnostics: &DeploymentDiagnostics, line: &[u8], truncated: bool) {
    let mut text = String::from_utf8_lossy(line).into_owned();
    if truncated {
        text.push_str(" [command output truncated]");
    }
    diagnostics.message(text);
}

#[cfg(test)]
mod tests;
