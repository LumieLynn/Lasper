use super::{
    persist_applying, persist_committed, send_deploy_log, AppliedResource, ApplyReport,
    DirectProvisioningCapabilities,
};
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::storage::StorageBackend;
use crate::application::provisioning::{
    DeploymentEvent as DeployLogEvent, DeploymentJobContext, DeploymentResource, DeploymentStage,
    ResourceDisposition,
};
use tokio::sync::mpsc::Sender;

pub(super) async fn inspect_deployment_sidecars(
    name: &str,
    host: &DirectProvisioningCapabilities,
) -> Result<Vec<String>> {
    if let Some(config) = host.nspawn.inspect(name).await? {
        return Err(NspawnError::Validation(format!(
            "Deployment target has existing .nspawn configuration: {}",
            config.path.display()
        )));
    }
    let mut warnings = Vec::new();
    match host.nvidia_state.read(name).await {
        Ok(Some(_)) => warnings.push(format!(
            "Deployment target {name:?} has existing NVIDIA state; it will be replaced only when current Lasper ownership can be proven."
        )),
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "Existing NVIDIA state could not be safely inspected and will be preserved: {error}"
        )),
    }

    match host.systemd_unit.read(name).await {
        Ok(unit) => {
            for drop_in in unit.drop_ins {
                let file_name = std::path::Path::new(&drop_in.path)
                    .file_name()
                    .and_then(|name| name.to_str());
                if file_name == Some("90-lasper.conf") {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be replaced only when current Lasper ownership can be proven.",
                        drop_in.path
                    ));
                } else {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be preserved.",
                        drop_in.path
                    ));
                }
            }
        }
        Err(error) => warnings.push(format!(
            "Existing systemd service drop-ins could not be safely inspected and will be preserved unless a validated Lasper-owned target is updated: {error}"
        )),
    }
    Ok(warnings)
}

pub(super) async fn rollback_apply_report(
    name: &str,
    report: &mut ApplyReport,
    storage: &dyn StorageBackend,
    host: &DirectProvisioningCapabilities,
    logs: &Sender<DeployLogEvent>,
    job: &DeploymentJobContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut reload_daemon = false;

    for resource in report.typed_owned_in_reverse() {
        send_deploy_log(logs, format!("Rolling back {}...", resource.label())).await;
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
        let result = match &resource {
            DeploymentResource::SystemdOverride(_) => {
                reload_daemon = true;
                host.systemd_unit.remove_service_override(name).await
            }
            DeploymentResource::NspawnConfig(_) => host.nspawn.remove(name).await,
            DeploymentResource::NvidiaState(_) => host.nvidia_state.remove(name).await,
            DeploymentResource::ExternalImage(_) => {
                let blockers = report.removal_blockers(AppliedResource::ExternalImage);
                if blockers.is_empty() {
                    host.system_operations.remove_image(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
                        "external image removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            DeploymentResource::LocalStorage(_) => {
                let blockers = report.removal_blockers(AppliedResource::LocalStorage);
                if blockers.is_empty() {
                    storage.delete(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
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
