//! Compensate resources whose ownership was established by this deployment.

use super::journal::{persist_applying, persist_committed};
use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::failure::OperationFailure;
use crate::adapters::provisioning::engine::host::{DirectProvisioningCapabilities, HostResultExt};
use crate::adapters::provisioning::engine::resources::{AppliedResource, ApplyReport};
use crate::adapters::storage::StorageBackend;
use crate::application::provisioning::{
    DeploymentJobContext, DeploymentResource, DeploymentStage, ResourceDisposition,
};

pub(super) async fn rollback_apply_report(
    name: &str,
    report: &mut ApplyReport,
    storage: Option<&dyn StorageBackend>,
    host: &DirectProvisioningCapabilities,
    logs: &DeploymentDiagnostics,
    job: &DeploymentJobContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut reload_daemon = false;

    for resource in report.typed_owned_in_reverse() {
        logs.message(format!("Rolling back {}...", resource.label()));
        if let Err(error) = persist_applying(
            job,
            DeploymentStage::Rollback,
            vec![resource.clone()],
            report,
        )
        .await
        {
            report.record_typed(resource, ResourceDisposition::CleanupPending);
            errors.push(format!("rollback was not attempted: {error}"));
            break;
        }
        let result: Result<(), OperationFailure> = match &resource {
            DeploymentResource::SystemdOverride(_) => {
                reload_daemon = true;
                host.systemd_unit
                    .remove_service_override(name)
                    .await
                    .host_context("remove deployment service override")
            }
            DeploymentResource::NspawnConfig(_) => host
                .nspawn
                .remove(name)
                .await
                .host_context("remove deployment nspawn configuration"),
            DeploymentResource::NvidiaState(_) => host
                .nvidia_state
                .remove(name)
                .await
                .host_context("remove deployment NVIDIA state"),
            DeploymentResource::ExternalImage(_) => {
                let blockers = report.removal_blockers(AppliedResource::ExternalImage);
                if blockers.is_empty() {
                    host.system_operations
                        .remove_image(name)
                        .await
                        .map_err(OperationFailure::from)
                } else {
                    Err(OperationFailure::InconsistentState(format!(
                        "external image removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            DeploymentResource::LocalStorage(_) => {
                let blockers = report.removal_blockers(AppliedResource::LocalStorage);
                if blockers.is_empty() {
                    match storage {
                        Some(storage) => storage
                            .delete(name)
                            .await
                            .host_context("remove deployment storage"),
                        None => Err(OperationFailure::InconsistentState(
                            "local storage was recorded without a storage backend".into(),
                        )),
                    }
                } else {
                    Err(OperationFailure::InconsistentState(format!(
                        "local storage removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            _ => continue,
        };
        match result {
            Ok(()) => {
                report.remove_typed(&resource);
                match resource {
                    DeploymentResource::ExternalImage(_) => {
                        report.remove_external_image_dependents();
                    }
                    DeploymentResource::LocalStorage(_) => report.remove_rootfs_dependents(),
                    _ => {}
                }
                if let Err(error) = persist_committed(job, DeploymentStage::Rollback, report).await
                {
                    report.record_typed(resource.clone(), ResourceDisposition::CleanupPending);
                    errors.push(format!(
                        "{} durable cleanup state: {error}",
                        resource.label()
                    ));
                    break;
                }
            }
            Err(error) => {
                report.record_typed(resource.clone(), ResourceDisposition::CleanupPending);
                errors.push(format!("{}: {error}", resource.label()));
            }
        }
    }

    if reload_daemon {
        if let Err(error) = host.system_operations.reload_daemon().await {
            errors.push(format!("systemd daemon reload: {error}"));
        }
    }
    errors
}
