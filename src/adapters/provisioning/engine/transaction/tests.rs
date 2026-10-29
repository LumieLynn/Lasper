use super::*;
use crate::adapters::process::MockCommandRunner;
use crate::adapters::provisioning::engine::failure::OperationFailure;
use crate::adapters::provisioning::engine::resources::AppliedResource;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::{
    deployment_job_channel, DeploymentId, DeploymentManifestState, DeploymentPlan,
    DeploymentResource, DeploymentStage, DeploymentStatePort, DeploymentStateSession,
    MemoryDeploymentStatePort, ResourceDisposition, ResourceLedger,
};
use crate::domain::provisioning::{CloneDeployment, DeploymentSpec};
use std::os::unix::process::ExitStatusExt;
use std::sync::Arc;

#[tokio::test]
async fn rejected_owned_images_are_rolled_back_or_retained_with_cleanup_errors() {
    for removal_succeeds in [true, false] {
        let mut commands = MockCommandRunner::new();
        commands
            .expect_run()
            .withf(|program, args| program == "machinectl" && args == &["--", "remove", "test"])
            .once()
            .returning(move |_, _| {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(if removal_succeeds {
                        0
                    } else {
                        1 << 8
                    }),
                    stdout: Vec::new(),
                    stderr: if removal_succeeds {
                        Vec::new()
                    } else {
                        b"image still busy".to_vec()
                    },
                })
            });
        let cause = OperationFailure::InvalidInput("image validation rejected the rootfs".into());
        let (error, state) = finish_owned_image(cause, commands).await;

        assert!(error
            .to_string()
            .contains("image validation rejected the rootfs"));
        let manifests = state.unfinished().await.unwrap();
        if removal_succeeds {
            assert!(!error.requires_reconciliation());
            assert!(manifests.is_empty());
        } else {
            assert!(error.requires_reconciliation());
            assert!(error.to_string().contains("image still busy"));
            assert_eq!(manifests.len(), 1);
            assert!(matches!(
                manifests[0].state,
                DeploymentManifestState::CleanupPending
            ));
            assert_eq!(
                manifests[0].committed_ledger.entries()[0].disposition,
                ResourceDisposition::CleanupPending
            );
        }
    }
}

#[tokio::test]
async fn unconfirmed_validator_exit_preserves_the_owned_image_for_reconciliation() {
    // No removal is permitted until process completion has been established.
    let cause = OperationFailure::process_exit_unconfirmed(
        "systemd-dissect",
        std::io::Error::other("wait failed"),
    );
    let (error, state) = finish_owned_image(cause, MockCommandRunner::new()).await;

    assert!(error.requires_reconciliation());
    let manifests = state.unfinished().await.unwrap();
    assert_eq!(manifests.len(), 1);
    assert!(matches!(
        manifests[0].state,
        DeploymentManifestState::CleanupPending
    ));
    assert_eq!(
        manifests[0].committed_ledger.entries()[0].disposition,
        ResourceDisposition::Created
    );
}

async fn finish_owned_image(
    cause: OperationFailure,
    commands: MockCommandRunner,
) -> (
    crate::application::provisioning::DeploymentError,
    Arc<MemoryDeploymentStatePort>,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = DirectProvisioningCapabilities::from_direct(
        Arc::new(commands),
        Arc::new(crate::adapters::process::MockRootfsProcessRunner::new()),
        TrustedStateRoot::for_test(directory.path().join("state")),
    );
    let plan = DeploymentPlan::build(DeploymentSpec::Clone(CloneDeployment {
        name: "test".into(),
        source_name: "base".into(),
    }))
    .unwrap();
    let id = DeploymentId::from_u128(1);
    let state = Arc::new(MemoryDeploymentStatePort::default());
    let session = DeploymentStateSession::new(state.clone(), id, &plan);
    session.prepare().await.unwrap();
    session
        .applying(
            DeploymentStage::SourceDeployment,
            vec![DeploymentResource::ExternalImage(plan.target().clone())],
            &ResourceLedger::default(),
        )
        .await
        .unwrap();
    let (_handle, job) = deployment_job_channel(id);
    let mut logs = DeploymentDiagnostics::new(id, plan.target().clone(), job.event_sender());
    let mut report = ApplyReport::new(plan.target().clone());
    report.record_created(AppliedResource::ExternalImage);
    let transaction = Transaction {
        method: DeploymentMethod::Clone(
            crate::adapters::provisioning::engine::sources::CloneDeployer {
                source_name: "base".into(),
                system_operations: host.system_operations().clone(),
                nspawn: host.nspawn().clone(),
                systemd_unit: host.systemd_unit().clone(),
            },
        ),
        host,
        job: job.with_state_session(session),
        logs: &mut logs,
        report,
        output: DeploymentResult::default(),
        raw_mount: None,
        storage_mount_attempted: false,
        external_provider_started: true,
    };
    let error = cleanup::finish(Err(cause), transaction).await.unwrap_err();
    (error.into_application_error(), state)
}
