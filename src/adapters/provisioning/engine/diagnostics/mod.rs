//! Execution logs and best-effort delivery of frontend events.

use crate::application::provisioning::{
    DeploymentError, DeploymentEvent, DeploymentId, DeploymentProgress, DeploymentResult,
    DeploymentStage,
};
use crate::domain::machine::MachineName;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc::{error::TrySendError, Sender};

pub(crate) struct DeploymentDiagnostics {
    id: DeploymentId,
    target: MachineName,
    stage: DeploymentStage,
    events: Sender<DeploymentEvent>,
    omitted_lines: AtomicUsize,
}

impl DeploymentDiagnostics {
    pub(crate) fn new(
        id: DeploymentId,
        target: MachineName,
        events: Sender<DeploymentEvent>,
    ) -> Self {
        Self {
            id,
            target,
            stage: DeploymentStage::Validation,
            events,
            omitted_lines: AtomicUsize::new(0),
        }
    }

    pub(crate) fn enter_stage(&mut self, stage: DeploymentStage) {
        self.stage = stage;
        self.record(log::Level::Info, "Stage started");
    }

    pub(super) fn message(&self, message: impl Into<String>) {
        let message = message.into();
        let level = if is_high_signal(&message) {
            log::Level::Warn
        } else {
            log::Level::Info
        };
        self.write(level, message);
    }

    pub(super) fn error(&self, message: impl Into<String>) {
        self.write(log::Level::Error, message.into());
    }

    pub(super) fn progress(&self, label: impl Into<String>, permille: u16) {
        // Progress is replaceable; it must not hold up command output or cancellation.
        let _ = self
            .events
            .try_send(DeploymentEvent::Progress(DeploymentProgress::new(
                label, permille,
            )));
    }

    pub(crate) fn finish(&self, result: &Result<DeploymentResult, DeploymentError>) {
        match result {
            Ok(_) => self.write(log::Level::Info, "Deployment complete.".into()),
            Err(error) if error.is_cancelled() => self.message(error.to_string()),
            Err(error) => self.error(format!("Deployment failed: {error}")),
        }
    }

    fn write(&self, level: log::Level, message: String) {
        // Persistence belongs to the executor, regardless of UI lifetime or queue capacity.
        self.record(level, &message);

        self.publish_line(message);
    }

    fn record(&self, level: log::Level, message: &str) {
        log::log!(
            level,
            "[DEPLOY {}] [Container: {}] [Stage: {:?}] {}",
            self.id,
            self.target,
            self.stage,
            message
        );
    }

    fn publish_line(&self, message: String) {
        let omitted = self.omitted_lines.swap(0, Ordering::Relaxed);
        if omitted > 0 {
            let notice = DeploymentEvent::Line(format!(
                "{omitted} deployment log messages omitted from this view; see the execution log."
            ));
            match self.events.try_send(notice) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.omitted_lines.fetch_add(omitted + 1, Ordering::Relaxed);
                    return;
                }
                Err(TrySendError::Closed(_)) => return,
            }
        }
        if matches!(
            self.events.try_send(DeploymentEvent::Line(message)),
            Err(TrySendError::Full(_))
        ) {
            self.omitted_lines.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) fn is_high_signal(message: &str) -> bool {
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

#[cfg(test)]
mod tests;
