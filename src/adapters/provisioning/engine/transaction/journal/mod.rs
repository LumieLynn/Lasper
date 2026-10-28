//! Write-ahead deployment state and recovery of interrupted effects.

use crate::adapters::provisioning::engine::resources::ApplyReport;
use crate::application::provisioning::{
    DeploymentJobContext, DeploymentResource, DeploymentStage, DeploymentStateError,
};

pub(super) async fn persist_applying(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    intended_resources: Vec<DeploymentResource>,
    report: &ApplyReport,
) -> Result<(), DeploymentStateError> {
    if let Some(state) = job.state_session() {
        state
            .applying(stage, intended_resources, report.application_ledger())
            .await?;
    }
    Ok(())
}

pub(super) async fn persist_committed(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    report: &ApplyReport,
) -> Result<(), DeploymentStateError> {
    if let Some(state) = job.state_session() {
        state.committed(stage, report.application_ledger()).await?;
    }
    Ok(())
}

pub(super) async fn persist_cleanup_pending(
    job: &DeploymentJobContext,
    report: &ApplyReport,
) -> Result<(), DeploymentStateError> {
    if let Some(state) = job.state_session() {
        state.cleanup_pending(report.application_ledger()).await?;
    }
    Ok(())
}

pub(super) async fn finish_manifest(
    job: &DeploymentJobContext,
) -> Result<(), DeploymentStateError> {
    if let Some(state) = job.state_session() {
        state.finish().await?;
    }
    Ok(())
}

pub(super) async fn capture_uncommitted_effects(
    job: &DeploymentJobContext,
    report: &mut ApplyReport,
) {
    let Some(state) = job.state_session() else {
        return;
    };
    for resource in state.current_applying_resources().await {
        if matches!(resource, DeploymentResource::RawConfigurationMount(_))
            && report.ledger.disposition(&resource).is_none()
        {
            report.block_storage_removal(
                "raw image configuration mount outcome requires reconciliation",
            );
        }
        report.record_outcome_unknown_if_unclassified(resource);
    }
}

#[cfg(test)]
mod tests;
