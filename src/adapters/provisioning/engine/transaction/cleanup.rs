//! Settle temporary access, compensate failed effects, and finalize durable job state.

use super::{
    journal::{
        capture_uncommitted_effects, finish_manifest, persist_applying, persist_cleanup_pending,
        persist_committed,
    },
    rollback::rollback_apply_report,
    Transaction,
};
use crate::adapters::provisioning::engine::failure::{
    DeploymentFailure, OperationFailure, OperationResult,
};
use crate::adapters::provisioning::engine::resources::AppliedResource;
use crate::application::provisioning::{
    DeploymentResource, DeploymentResult, DeploymentStage, ResourceDisposition,
};

pub(super) async fn finish(
    result: OperationResult<()>,
    transaction: Transaction<'_>,
) -> Result<DeploymentResult, DeploymentFailure> {
    let Transaction {
        host,
        method,
        job,
        logs,
        mut report,
        output,
        raw_mount,
        storage_mount_attempted,
        external_provider_started,
    } = transaction;
    let name = report.target.to_string();
    let cancellation = job.cancellation();
    if let Err(error) = &result {
        if matches!(error, OperationFailure::Cancelled) {
            logs.message("Cancellation requested.");
        } else {
            logs.error(format!("Stage failed: {error}"));
        }
        capture_uncommitted_effects(&job, &mut report).await;
    }

    if result
        .as_ref()
        .err()
        .is_some_and(OperationFailure::outcome_is_unconfirmed)
    {
        let error = result.unwrap_err();
        let message =
            "mounts and resources were preserved because the operation outcome is unconfirmed";
        logs.warning(message);
        let mut cleanup_errors = vec![message.into()];
        if let Err(state_error) = persist_cleanup_pending(&job, &report).await {
            cleanup_errors.push(format!("cleanup-pending state: {state_error}"));
        }
        return Err(DeploymentFailure::new(error, cleanup_errors));
    }

    logs.enter_stage(DeploymentStage::Cleanup);
    let mut cleanup_errors = Vec::new();
    let mut durable_cleanup_failed = false;
    if let Some(mount) = raw_mount {
        logs.message("Unmounting raw image...".to_string());
        let resource = DeploymentResource::RawConfigurationMount(report.target.clone());
        match persist_applying(
            &job,
            DeploymentStage::Cleanup,
            vec![resource.clone()],
            &report,
        )
        .await
        {
            Ok(()) => match mount.unmount(host.commands.as_ref()).await {
                Ok(()) => {
                    report.remove_typed(&resource);
                    if let Err(error) =
                        persist_committed(&job, DeploymentStage::Cleanup, &report).await
                    {
                        let message = format!("raw mount cleanup state: {error}");
                        durable_cleanup_failed = true;
                        report.record_typed(resource, ResourceDisposition::CleanupPending);
                        report.block_storage_removal(message.clone());
                        cleanup_errors.push(message);
                    }
                }
                Err(error) => {
                    let message = format!("raw image configuration mount: {error}");
                    logs.warning(format!("Failed to clean up {message}"));
                    report.record_typed(resource, ResourceDisposition::CleanupPending);
                    report.block_storage_removal(message.clone());
                    cleanup_errors.push(message);
                }
            },
            Err(error) => {
                let message = format!("raw mount cleanup was not attempted: {error}");
                durable_cleanup_failed = true;
                report.record_typed(resource, ResourceDisposition::CleanupPending);
                report.block_storage_removal(message.clone());
                cleanup_errors.push(message);
            }
        }
    }

    if let Some(storage) = method.storage().filter(|_| storage_mount_attempted) {
        logs.message("Cleaning up storage mount...".to_string());
        let resource = DeploymentResource::StorageMount(report.target.clone());
        if durable_cleanup_failed {
            let message =
                "storage cleanup was not attempted after durable cleanup state failed".to_string();
            report.record_typed(resource, ResourceDisposition::CleanupPending);
            report.block_storage_removal(message.clone());
            cleanup_errors.push(message);
        } else {
            match persist_applying(
                &job,
                DeploymentStage::Cleanup,
                vec![resource.clone()],
                &report,
            )
            .await
            {
                Ok(()) => match storage.unmount(&name).await {
                    Ok(()) => {
                        report.remove_typed(&resource);
                        if let Err(error) =
                            persist_committed(&job, DeploymentStage::Cleanup, &report).await
                        {
                            let message = format!("storage cleanup state: {error}");
                            durable_cleanup_failed = true;
                            report.record_typed(resource, ResourceDisposition::CleanupPending);
                            report.block_storage_removal(message.clone());
                            cleanup_errors.push(message);
                        }
                    }
                    Err(error) => {
                        let message = format!("storage unmount: {error}");
                        logs.warning(format!("Failed to clean up {message}"));
                        report.record_typed(resource, ResourceDisposition::CleanupPending);
                        report.block_storage_removal(message.clone());
                        cleanup_errors.push(message);
                    }
                },
                Err(error) => {
                    let message = format!("storage cleanup was not attempted: {error}");
                    durable_cleanup_failed = true;
                    report.record_typed(resource, ResourceDisposition::CleanupPending);
                    report.block_storage_removal(message.clone());
                    cleanup_errors.push(message);
                }
            }
        }
    }

    let result = if result.is_ok() && cancellation.is_requested() {
        Err(OperationFailure::Cancelled)
    } else if result.is_ok() && !cleanup_errors.is_empty() {
        Err(OperationFailure::CleanupFailed)
    } else {
        result
    };

    if let Err(error) = result {
        logs.message(format!("Deployment stopped: {}", error));
        job.set_rolling_back(true);
        logs.enter_stage(DeploymentStage::Rollback);
        logs.message("Rolling back resources created by this deployment...".to_string());

        let external_ownership_confirmed = report.owns(AppliedResource::ExternalImage);
        let mut rollback_errors = cleanup_errors;
        if durable_cleanup_failed {
            rollback_errors
                .push("rollback was not attempted after durable cleanup state failed".into());
        } else {
            rollback_errors.extend(
                rollback_apply_report(&name, &mut report, method.storage(), &host, logs, &job)
                    .await,
            );
        }

        if external_provider_started && !external_ownership_confirmed {
            let warning = format!(
                "external provider did not confirm ownership of image {name:?}; any partial provider output was preserved for manual inspection"
            );
            logs.warning(format!("WARNING: {warning}"));
        }

        for resource in report.outcome_unknown_resources() {
            rollback_errors.push(format!(
                "{} outcome requires authoritative reconciliation",
                resource.label()
            ));
        }

        job.set_rolling_back(false);
        if rollback_errors.is_empty() {
            match finish_manifest(&job).await {
                Ok(()) => {
                    logs.message("Rollback complete.");
                }
                Err(manifest_error) => {
                    rollback_errors.push(format!("deployment crash manifest: {manifest_error}"))
                }
            }
        } else if let Err(state_error) = persist_cleanup_pending(&job, &report).await {
            rollback_errors.push(format!("deployment cleanup state: {state_error}"));
        }
        for rollback_error in &rollback_errors {
            logs.error(format!("ROLLBACK ERROR: {rollback_error}"));
        }
        return Err(DeploymentFailure::new(error, rollback_errors));
    }

    finish_manifest(&job).await.map_err(|error| {
        DeploymentFailure::new(
            error.into(),
            vec!["durable deployment state could not be finalized".into()],
        )
    })?;
    Ok(output)
}
