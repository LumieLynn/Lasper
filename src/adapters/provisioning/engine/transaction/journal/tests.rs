use super::*;
use crate::application::provisioning::{
    deployment_job_channel, DeploymentId, DeploymentPlan, DeploymentStatePort,
    DeploymentStateSession, MemoryDeploymentStatePort, ResourceDisposition, ResourceLedger,
};
use crate::domain::provisioning::DeploymentSpec;
use std::sync::Arc;

#[tokio::test]
async fn interrupted_applying_effect_is_persisted_as_unknown_not_owned() {
    let plan = DeploymentPlan::build(DeploymentSpec::Clone(
        crate::domain::provisioning::CloneDeployment {
            name: "test".into(),
            source_name: "base".into(),
        },
    ))
    .unwrap();
    let id = DeploymentId::from_u128(42);
    let state = Arc::new(MemoryDeploymentStatePort::default());
    let session = DeploymentStateSession::new(state.clone(), id, &plan);
    session.prepare().await.unwrap();
    let resource = DeploymentResource::RawConfigurationMount(plan.target().clone());
    session
        .applying(
            DeploymentStage::RootfsMutation,
            vec![resource.clone()],
            &ResourceLedger::default(),
        )
        .await
        .unwrap();
    let (_handle, job) = deployment_job_channel(id);
    let job = job.with_state_session(session);
    let mut report = ApplyReport::new(plan.target().clone());

    capture_uncommitted_effects(&job, &mut report).await;
    persist_cleanup_pending(&job, &report).await.unwrap();

    assert_eq!(
        report.ledger.disposition(&resource),
        Some(ResourceDisposition::OutcomeUnknown)
    );
    assert!(!report.ledger.owns(&resource));
    assert_eq!(report.storage_removal_blockers.len(), 1);
    let manifests = state.unfinished().await.unwrap();
    assert_eq!(manifests.len(), 1);
    assert!(matches!(
        manifests[0].state,
        crate::application::provisioning::DeploymentManifestState::CleanupPending
    ));
    assert_eq!(
        manifests[0].committed_ledger.entries()[0].disposition,
        ResourceDisposition::OutcomeUnknown
    );
}
