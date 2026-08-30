use super::stream::is_high_signal_deploy_stream;
use super::*;
use crate::application::provisioning::ResourceApplyStatus;
use crate::application::provisioning::{
    deployment_job_channel, DeploymentId, DeploymentPlan, DeploymentRequest, DeploymentResource,
    DeploymentSource, DeploymentStage, DeploymentStatePort, DeploymentStateSession,
    DeploymentStorage, MachineProvisioningConfig, MemoryDeploymentStatePort, ResourceDisposition,
    ResourceLedger,
};
use crate::nspawn::errors::{NspawnError, Result};
use std::sync::Arc;

fn apply_report() -> ApplyReport {
    ApplyReport::new(crate::domain::machine::MachineName::new("test").unwrap())
}

#[test]
fn deploy_stream_classifies_recoverable_bootstrap_diagnostics() {
    for message in [
        "W: Failure trying to run: test-dev-null",
        "test-dev-null: Permission denied",
        "E: bootstrap failed",
        "operation not permitted while probing target",
    ] {
        assert!(
            is_high_signal_deploy_stream(message),
            "did not classify {message:?}"
        );
    }
}

#[test]
fn deploy_stream_distinguishes_normal_output_from_warnings() {
    for message in [
        "I: Retrieving base-files",
        "I: Extracting base-passwd",
        "Download complete",
    ] {
        assert!(
            !is_high_signal_deploy_stream(message),
            "misclassified {message:?}"
        );
    }
}

#[test]
fn apply_report_records_only_resources_created_by_this_attempt() {
    let mut report = apply_report();
    report
        .record_apply(AppliedResource::NspawnConfig, ResourceApplyStatus::Created)
        .unwrap();
    report
        .record_apply(AppliedResource::NspawnConfig, ResourceApplyStatus::Created)
        .unwrap();
    report
        .record_apply(
            AppliedResource::SystemdOverride,
            ResourceApplyStatus::Unchanged,
        )
        .unwrap();

    assert!(report.owns(AppliedResource::NspawnConfig));
    assert!(!report.owns(AppliedResource::SystemdOverride));
}

#[test]
fn unknown_nspawn_owner_blocks_external_image_compensation() {
    let mut report = apply_report();
    let error = report
        .record_apply(
            AppliedResource::NspawnConfig,
            ResourceApplyStatus::ConflictUnknownOwner,
        )
        .unwrap_err();

    assert!(error.to_string().contains("unknown ownership"));
    assert!(!report.owns(AppliedResource::NspawnConfig));
    assert_eq!(report.external_image_blockers.len(), 1);
}

#[test]
fn sidecar_conflicts_are_preserved_and_owned_replacements_are_adopted() {
    let mut report = apply_report();
    report
        .record_apply(
            AppliedResource::NvidiaState,
            ResourceApplyStatus::ConflictUnknownOwner,
        )
        .unwrap();
    report
        .record_apply(
            AppliedResource::SystemdOverride,
            ResourceApplyStatus::ReplacedOwned,
        )
        .unwrap();

    assert!(report.owns(AppliedResource::SystemdOverride));
    assert!(!report.owns(AppliedResource::NvidiaState));
    assert!(report.external_image_blockers.is_empty());
}

#[test]
fn unknown_effects_are_not_rolled_back_as_owned_resources() {
    let mut report = apply_report();
    let unknown = DeploymentResource::NspawnConfig(report.target.clone());
    report.record_outcome_unknown_if_unclassified(unknown.clone());

    assert_eq!(report.outcome_unknown_resources(), vec![unknown]);
    assert!(!report.owns(AppliedResource::NspawnConfig));
    assert!(report.typed_owned_in_reverse().is_empty());
}

#[test]
fn removing_owned_storage_resolves_unknown_rootfs_effects() {
    let mut report = apply_report();
    report.record_created(AppliedResource::LocalStorage);
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsAccounts(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsHostname(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNetwork(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNvidia(
        report.target.clone(),
    ));

    report.remove_rootfs_dependents();

    assert!(report.outcome_unknown_resources().is_empty());
    assert!(report.owns(AppliedResource::LocalStorage));
}

#[tokio::test]
async fn interrupted_applying_effect_is_persisted_as_unknown_not_owned() {
    let plan = DeploymentPlan::build(DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "test".into(),
            ..Default::default()
        },
        source: DeploymentSource::Copy {
            source_name: "base".into(),
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: Vec::new(),
        allow_unsafe_remote_tar: false,
    })
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

#[test]
fn failed_unmount_blocks_local_and_external_storage_compensation() {
    let mut report = apply_report();
    report
        .external_image_blockers
        .push("unknown .nspawn owner".into());
    report.block_storage_removal("storage is still mounted");

    assert_eq!(
        report.removal_blockers(AppliedResource::LocalStorage),
        vec!["storage is still mounted"]
    );
    assert_eq!(
        report.removal_blockers(AppliedResource::ExternalImage),
        vec!["unknown .nspawn owner", "storage is still mounted"]
    );
}

#[tokio::test]
async fn deployment_cancellation_notifies_waiters_and_fails_checkpoints() {
    let cancellation = DeploymentCancellation::default();
    let waiter = cancellation.clone();
    let task = tokio::spawn(async move { waiter.cancelled().await });

    cancellation.request();
    task.await.unwrap();
    let result: Result<()> = cancellation.checkpoint().map_err(Into::into);
    assert!(matches!(result, Err(NspawnError::DeploymentCancelled)));
}

#[tokio::test]
async fn failed_process_wait_is_not_treated_as_a_rollback_safe_failure() {
    let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
        Box::new(tokio::io::empty()),
        async { Err(std::io::Error::other("wait channel closed")) },
        |_| Box::pin(async { Ok(()) }),
    );
    let (logs, _receiver) = tokio::sync::mpsc::channel(4);

    let error = stream_deploy_command(
        spawned,
        &logs,
        &DeploymentCancellation::default(),
        "test deployer",
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        NspawnError::DeploymentProcessStateUnknown(message)
            if message.contains("test deployer") && message.contains("wait channel closed")
    ));
}

#[tokio::test]
async fn authoritative_completion_wins_a_racing_cancellation() {
    use std::os::unix::process::ExitStatusExt;

    let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
        Box::new(tokio::io::empty()),
        async { Ok(std::process::ExitStatus::from_raw(0)) },
        |_| Box::pin(async { Ok(()) }),
    )
    .with_completion_wins_cancellation();
    let cancellation = DeploymentCancellation::default();
    cancellation.request();
    let (logs, _receiver) = tokio::sync::mpsc::channel(4);

    let status = stream_deploy_command(spawned, &logs, &cancellation, "authoritative transfer")
        .await
        .unwrap();

    assert!(status.success());
}

#[test]
fn only_confirmed_cancellation_outcomes_are_reported_as_cancelled() {
    assert!(is_cancelled_outcome(&NspawnError::DeploymentCancelled));
    assert!(is_cancelled_outcome(
        &NspawnError::DeploymentCancellationRollbackIncomplete("cleanup failed".into())
    ));
    assert!(!is_cancelled_outcome(
        &NspawnError::DeploymentProcessStateUnknown("still running".into())
    ));
}
