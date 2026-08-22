//! Session-scoped client for daemon-owned provisioning jobs.

use crate::adapters::elevated::{pipe_reader, ElevatedDaemon};
use crate::application::provisioning::{
    DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentPlan, DeploymentRequestId,
    DeploymentSecrets, DeploymentSource, DeploymentStatus,
};
use crate::daemon::deployment_protocol::{
    DeploymentClaimState, DeploymentJobSnapshot, DeploymentStreamFrame, DeploymentSubmissionStatus,
    SubmitDeploymentParams, MAX_DEPLOYMENT_STREAM_FRAME_BYTES,
};
use crate::daemon::transport::read_bounded_line;
use async_trait::async_trait;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct ElevatedProvisioningExecutor {
    daemon: Arc<ElevatedDaemon>,
}

impl ElevatedProvisioningExecutor {
    pub(crate) fn new(daemon: Arc<ElevatedDaemon>) -> Self {
        Self { daemon }
    }

    async fn open_artifact(
        source: &DeploymentSource,
    ) -> Result<Option<std::fs::File>, DeploymentError> {
        let DeploymentSource::Artifact(artifact) = source else {
            return Ok(None);
        };
        let path = artifact.expanded_path();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path).map_err(|error| {
                DeploymentError::failed(format!(
                    "Could not open deployment artifact {path:?}: {error}"
                ))
            })?;
            crate::adapters::storage::image_ops::validate_import_source(&file).map_err(
                |error| {
                    DeploymentError::failed(format!(
                        "Could not validate deployment artifact {path:?}: {error}"
                    ))
                },
            )?;
            Ok(file)
        })
        .await
        .map_err(|error| {
            DeploymentError::failed(format!("Artifact validation task failed: {error}"))
        })?
        .map(Some)
    }

    async fn resolve_submission(
        &self,
        request_id: DeploymentRequestId,
        deployment_id: crate::application::provisioning::DeploymentId,
        submission_error: std::io::Error,
    ) -> Result<Option<RawFd>, DeploymentError> {
        loop {
            let snapshot = self
                .daemon
                .resolve_deployment_submission(request_id)
                .await
                .map_err(|status_error| {
                    DeploymentError::reconciliation_required(format!(
                        "Deployment submission outcome is unknown: {submission_error}; resolution failed: {status_error}"
                    ))
                })?;
            let Some(snapshot) = snapshot else {
                return Err(DeploymentError::failed(format!(
                    "Daemon rejected deployment before enqueue: {submission_error}"
                )));
            };
            if snapshot.request_id != request_id {
                return Err(DeploymentError::reconciliation_required(format!(
                    "Daemon returned submission {} while resolving {request_id}",
                    snapshot.request_id
                )));
            }
            match snapshot.status {
                DeploymentSubmissionStatus::Pending => {
                    tokio::time::sleep(STATUS_POLL_INTERVAL).await;
                }
                DeploymentSubmissionStatus::Accepted {
                    deployment_id: accepted,
                } if accepted == deployment_id => {
                    log::warn!(
                        "Deployment {deployment_id} was accepted but its event stream could not be acquired: {submission_error}"
                    );
                    self.acknowledge_submission(request_id).await;
                    return Ok(None);
                }
                DeploymentSubmissionStatus::Accepted {
                    deployment_id: accepted,
                } => {
                    return Err(DeploymentError::reconciliation_required(format!(
                        "Submission {request_id} resolved to deployment {accepted} instead of {deployment_id}"
                    )));
                }
                DeploymentSubmissionStatus::Rejected { message } => {
                    self.acknowledge_submission(request_id).await;
                    return Err(DeploymentError::failed(format!(
                        "Daemon rejected deployment submission: {message}"
                    )));
                }
            }
        }
    }

    async fn acknowledge_submission(&self, request_id: DeploymentRequestId) {
        if let Err(error) = self
            .daemon
            .acknowledge_deployment_submission(request_id)
            .await
        {
            log::warn!("Could not acknowledge deployment submission {request_id}: {error}");
        }
    }

    async fn wait_for_terminal_status(
        &self,
        request_id: DeploymentRequestId,
        deployment_id: crate::application::provisioning::DeploymentId,
        context: &DeploymentJobContext,
    ) -> Result<(), DeploymentError> {
        loop {
            let snapshot = self
                .daemon
                .deployment_status(deployment_id)
                .await
                .map_err(|error| {
                    DeploymentError::reconciliation_required(format!(
                        "Could not resolve deployment {deployment_id} after its event stream closed: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    DeploymentError::reconciliation_required(format!(
                        "Daemon lost the accepted deployment record for {deployment_id}"
                    ))
                })?;
            validate_snapshot(deployment_id, &snapshot)?;
            if snapshot.status.is_finished() {
                return self.finish(request_id, snapshot).await;
            }
            context.status_sender().send_replace(snapshot.status);
            tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        }
    }

    async fn consume_stream(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
        stream_fd: RawFd,
        context: &DeploymentJobContext,
    ) -> Result<Option<DeploymentJobSnapshot>, DeploymentError> {
        let stream = pipe_reader(stream_fd).map_err(|error| {
            DeploymentError::reconciliation_required(format!(
                "Could not attach to deployment {deployment_id} event stream: {error}"
            ))
        })?;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut overflow_reported = false;
        let mut last_revision = 0;
        loop {
            let line = match read_bounded_line(&mut reader, MAX_DEPLOYMENT_STREAM_FRAME_BYTES).await
            {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(None),
                Err(error) => {
                    log::warn!(
                        "Deployment {deployment_id} event stream became unreadable; resolving through daemon status: {error}"
                    );
                    return Ok(None);
                }
            };
            let frame: DeploymentStreamFrame = serde_json::from_str(line.trim_end()).map_err(
                |error| {
                    DeploymentError::reconciliation_required(format!(
                        "Daemon sent an invalid deployment stream frame for {deployment_id}: {error}"
                    ))
                },
            )?;
            match frame {
                DeploymentStreamFrame::Event(event) => {
                    match context.event_sender().try_send(event) {
                        Ok(()) => overflow_reported = false,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            if !overflow_reported {
                                log::warn!(
                                    "Deployment {deployment_id} event buffer is full; dropping output until the consumer catches up"
                                );
                                overflow_reported = true;
                            }
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                    }
                }
                DeploymentStreamFrame::Snapshot(snapshot) => {
                    validate_snapshot(deployment_id, &snapshot)?;
                    if snapshot.revision < last_revision {
                        return Err(DeploymentError::reconciliation_required(format!(
                            "Deployment {deployment_id} stream revision regressed from {last_revision} to {}",
                            snapshot.revision
                        )));
                    }
                    last_revision = snapshot.revision;
                    if snapshot.status.is_finished() {
                        return Ok(Some(snapshot));
                    }
                    context.status_sender().send_replace(snapshot.status);
                }
            }
        }
    }

    async fn finish(
        &self,
        request_id: DeploymentRequestId,
        snapshot: DeploymentJobSnapshot,
    ) -> Result<(), DeploymentError> {
        self.acknowledge_submission(request_id).await;
        if snapshot.claim == DeploymentClaimState::ReconciliationRequired {
            let historical_message = match &snapshot.status {
                DeploymentStatus::ReconciliationRequired(message) => message.clone(),
                _ => {
                    return Err(DeploymentError::reconciliation_required(
                        "daemon retained a claim for a non-reconciliation terminal state",
                    ));
                }
            };
            let reconciled = self
                .daemon
                .reconcile_deployment(snapshot.deployment_id)
                .await
                .map_err(|error| {
                    DeploymentError::reconciliation_required(format!(
                        "{historical_message}; authoritative daemon reconciliation failed: {error}"
                    ))
                })?;
            validate_snapshot(snapshot.deployment_id, &reconciled)?;
            if reconciled.claim == DeploymentClaimState::ReconciliationRequired {
                return Err(DeploymentError::reconciliation_required(historical_message));
            }
            if reconciled.claim != DeploymentClaimState::Reconciled {
                return Err(DeploymentError::reconciliation_required(format!(
                    "{historical_message}; daemon returned an invalid reconciliation claim state"
                )));
            }
            if let Err(error) = self
                .daemon
                .acknowledge_deployment(reconciled.deployment_id)
                .await
            {
                log::warn!(
                    "Could not acknowledge reconciled deployment {}: {error}",
                    reconciled.deployment_id
                );
            }
            return Err(DeploymentError::reconciled_unknown(format!(
                "{historical_message}; trusted daemon state no longer contains an unfinished manifest"
            )));
        }
        if let Err(error) = self
            .daemon
            .acknowledge_deployment(snapshot.deployment_id)
            .await
        {
            log::warn!(
                "Could not acknowledge completed deployment {}: {error}",
                snapshot.deployment_id
            );
        }
        status_into_result(snapshot.status)
    }
}

#[async_trait]
impl DeploymentExecutor for ElevatedProvisioningExecutor {
    async fn run(
        &self,
        plan: DeploymentPlan,
        secrets: DeploymentSecrets,
        context: DeploymentJobContext,
    ) -> Result<(), DeploymentError> {
        let deployment_id = context.id();
        let request_id = DeploymentRequestId::new();
        let request = plan.into_request();
        let artifact_source = Self::open_artifact(&request.source).await?;
        let params = SubmitDeploymentParams {
            request_id,
            deployment_id,
            request,
            secrets: secrets.into_wire(),
        };
        let stream_fd = match self.daemon.submit_deployment(params, artifact_source).await {
            Ok(stream_fd) => {
                self.acknowledge_submission(request_id).await;
                Some(stream_fd)
            }
            Err(error) => {
                self.resolve_submission(request_id, deployment_id, error)
                    .await?
            }
        };

        let cancellation = context.cancellation();
        let daemon = Arc::clone(&self.daemon);
        let cancellation_task = tokio::spawn(async move {
            cancellation.cancelled().await;
            if let Err(error) = daemon.cancel_deployment(deployment_id).await {
                log::warn!("Could not cancel deployment {deployment_id}: {error}");
            }
        });

        let result = match stream_fd {
            Some(stream_fd) => match self
                .consume_stream(deployment_id, stream_fd, &context)
                .await
            {
                Ok(Some(snapshot)) => self.finish(request_id, snapshot).await,
                Ok(None) => {
                    self.wait_for_terminal_status(request_id, deployment_id, &context)
                        .await
                }
                Err(error) => {
                    log::warn!(
                        "Deployment {deployment_id} event stream failed; resolving through daemon status: {error}"
                    );
                    self.wait_for_terminal_status(request_id, deployment_id, &context)
                        .await
                }
            },
            None => {
                self.wait_for_terminal_status(request_id, deployment_id, &context)
                    .await
            }
        };
        cancellation_task.abort();
        result
    }
}

pub(super) fn validate_snapshot(
    expected: crate::application::provisioning::DeploymentId,
    snapshot: &DeploymentJobSnapshot,
) -> Result<(), DeploymentError> {
    if snapshot.deployment_id != expected {
        return Err(DeploymentError::reconciliation_required(format!(
            "Daemon returned deployment {} while resolving {expected}",
            snapshot.deployment_id
        )));
    }
    if snapshot.revision == 0 {
        return Err(DeploymentError::reconciliation_required(
            "Daemon returned a deployment snapshot with revision zero",
        ));
    }
    let claim_matches = match snapshot.status {
        DeploymentStatus::Running | DeploymentStatus::RollingBack => {
            snapshot.claim == DeploymentClaimState::Held
        }
        DeploymentStatus::ReconciliationRequired(_) => matches!(
            snapshot.claim,
            DeploymentClaimState::ReconciliationRequired
                | DeploymentClaimState::Reconciled
                | DeploymentClaimState::ReleasedUnresolved
        ),
        DeploymentStatus::Succeeded
        | DeploymentStatus::Failed(_)
        | DeploymentStatus::Cancelled(_) => snapshot.claim == DeploymentClaimState::Released,
    };
    if claim_matches {
        Ok(())
    } else {
        Err(DeploymentError::reconciliation_required(format!(
            "Daemon returned inconsistent status/claim state for deployment {expected}"
        )))
    }
}

fn status_into_result(status: DeploymentStatus) -> Result<(), DeploymentError> {
    match status {
        DeploymentStatus::Succeeded => Ok(()),
        DeploymentStatus::Failed(message) => Err(DeploymentError::failed(message)),
        DeploymentStatus::Cancelled(message) => Err(DeploymentError::cancelled(message)),
        DeploymentStatus::ReconciliationRequired(message) => {
            Err(DeploymentError::reconciliation_required(message))
        }
        DeploymentStatus::Running | DeploymentStatus::RollingBack => {
            Err(DeploymentError::reconciliation_required(
                "daemon reported a non-terminal result as terminal",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_statuses_preserve_their_application_meaning() {
        assert!(status_into_result(DeploymentStatus::Succeeded).is_ok());
        assert!(status_into_result(DeploymentStatus::Failed("failed".into())).is_err());
        assert!(
            status_into_result(DeploymentStatus::Cancelled("cancelled".into()))
                .unwrap_err()
                .is_cancelled()
        );
        assert!(status_into_result(DeploymentStatus::ReconciliationRequired(
            "inspect state".into()
        ))
        .unwrap_err()
        .requires_reconciliation());
    }

    #[test]
    fn non_terminal_status_cannot_complete_an_executor() {
        assert!(status_into_result(DeploymentStatus::Running)
            .unwrap_err()
            .requires_reconciliation());
        assert!(status_into_result(DeploymentStatus::RollingBack)
            .unwrap_err()
            .requires_reconciliation());
    }
}
