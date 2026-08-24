//! Route-specific release of unresolved deployment coordination claims.

use crate::adapters::elevated::ElevatedDaemon;
use crate::application::provisioning::{
    DeploymentClaimControl, DeploymentError, DeploymentId, DeploymentStatus,
};
use crate::daemon::protocol::deployment::DeploymentClaimState;
use async_trait::async_trait;
use std::sync::Arc;

pub(crate) struct DirectDeploymentClaimControl;

#[async_trait]
impl DeploymentClaimControl for DirectDeploymentClaimControl {
    async fn release_unresolved(
        &self,
        deployment_id: DeploymentId,
        confirmed: bool,
    ) -> Result<(), DeploymentError> {
        if !confirmed {
            return Err(DeploymentError::rejected(
                "releasing an unresolved deployment requires explicit confirmation",
            ));
        }
        log::warn!(
            "[AUDIT] Direct deployment {deployment_id} was explicitly released from session coordination; durable recovery state was retained"
        );
        Ok(())
    }
}

pub(crate) struct ElevatedDeploymentClaimControl {
    daemon: Arc<ElevatedDaemon>,
}

impl ElevatedDeploymentClaimControl {
    pub(crate) fn new(daemon: Arc<ElevatedDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl DeploymentClaimControl for ElevatedDeploymentClaimControl {
    async fn release_unresolved(
        &self,
        deployment_id: DeploymentId,
        confirmed: bool,
    ) -> Result<(), DeploymentError> {
        let snapshot = self
            .daemon
            .release_unresolved_deployment(deployment_id, confirmed)
            .await
            .map_err(|error| {
                DeploymentError::reconciliation_required(format!(
                    "daemon could not release deployment {deployment_id}: {error}"
                ))
            })?;
        super::elevated::validate_snapshot(deployment_id, &snapshot)?;
        if !matches!(snapshot.status, DeploymentStatus::ReconciliationRequired(_))
            || snapshot.claim != DeploymentClaimState::ReleasedUnresolved
        {
            return Err(DeploymentError::reconciliation_required(format!(
                "daemon returned an invalid unresolved-release result for deployment {deployment_id}"
            )));
        }
        if let Err(error) = self.daemon.acknowledge_deployment(deployment_id).await {
            log::warn!(
                "Daemon released deployment {deployment_id}, but its terminal record could not be acknowledged: {error}"
            );
        }
        Ok(())
    }
}
