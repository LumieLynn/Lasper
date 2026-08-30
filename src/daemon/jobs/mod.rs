//! Deployment Job-family daemon handlers.
//!
//! This module owns the lifecycle protocol around a deployment record. The
//! deployment executor itself remains in the provisioning application layer;
//! these handlers only inspect or transition daemon-owned job state.

pub(crate) mod server;

use super::dispatch::handler::{DaemonRuntimeQueries, HandleOutcome};
use super::server::DaemonServerState;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::DeploymentStatePort;
use crate::domain::runtime::ImageEntry;
use crate::ipc::protocol::deployment::{
    DeploymentClaimState, DeploymentJobRequest, DeploymentSubmissionRequest,
    ProbeDeploymentRecoveryRequest, ProbeDeploymentRecoveryResult,
    ReleaseUnresolvedDeploymentRequest,
};
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct JobContext<'a, B> {
    pub(super) params: Value,
    pub(super) dbus: &'a Option<B>,
    pub(super) server_state: Arc<DaemonServerState>,
    pub(super) trusted_state_root: TrustedStateRoot,
}

pub(crate) async fn handle<B: DaemonRuntimeQueries>(
    method: RpcMethod,
    context: JobContext<'_, B>,
) -> HandleOutcome {
    let JobContext {
        params,
        dbus,
        server_state,
        trusted_state_root,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Job);

    match method {
        RpcMethod::DeploymentStatus => {
            let request: DeploymentJobRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid deployment_status request: {error}"
                    )));
                }
            };
            match server_state.deployments.snapshot(request.deployment_id) {
                Ok(snapshot) => HandleOutcome::Sync(
                    serde_json::to_value(Some(snapshot)).map_err(|error| error.to_string()),
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    HandleOutcome::Sync(Ok(Value::Null))
                }
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::ResolveDeploymentSubmission => {
            let request: DeploymentSubmissionRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid resolve_deployment_submission request: {error}"
                    )));
                }
            };
            match server_state
                .deployments
                .resolve_submission(request.request_id)
            {
                Ok(snapshot) => HandleOutcome::Sync(
                    serde_json::to_value(Some(snapshot)).map_err(|error| error.to_string()),
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    HandleOutcome::Sync(Ok(Value::Null))
                }
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::AcknowledgeDeploymentSubmission => {
            let request: DeploymentSubmissionRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid acknowledge_deployment_submission request: {error}"
                    )));
                }
            };
            match server_state
                .deployments
                .acknowledge_submission(request.request_id)
            {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::json!({}))),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::CancelDeployment => {
            let request: DeploymentJobRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid cancel_deployment request: {error}"
                    )));
                }
            };
            match server_state.deployments.cancel(request.deployment_id) {
                Ok(snapshot) => HandleOutcome::Sync(
                    serde_json::to_value(snapshot).map_err(|error| error.to_string()),
                ),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::AcknowledgeDeployment => {
            let request: DeploymentJobRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid acknowledge_deployment request: {error}"
                    )));
                }
            };
            match server_state.deployments.acknowledge(request.deployment_id) {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::json!({}))),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::ProbeDeploymentRecovery => {
            let request: ProbeDeploymentRecoveryRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid probe_deployment_recovery request: {error}"
                    )));
                }
            };
            let state = crate::adapters::provisioning::state::FilesystemDeploymentState::new(
                trusted_state_root.clone(),
            );
            let manifests = match state.unfinished().await {
                Ok(manifests) => manifests,
                Err(error) => return HandleOutcome::Sync(Err(error.to_string())),
            };
            let manifest = match manifests
                .into_iter()
                .find(|manifest| manifest.deployment_id == request.deployment_id)
            {
                Some(manifest) if manifest.revision == request.expected_revision => manifest,
                Some(manifest) => {
                    return HandleOutcome::Sync(Err(format!(
                        "deployment {} manifest revision changed from {} to {}",
                        request.deployment_id, request.expected_revision, manifest.revision
                    )));
                }
                None => {
                    return HandleOutcome::Sync(Err(format!(
                        "deployment {} crash manifest is missing",
                        request.deployment_id
                    )));
                }
            };
            let _reservation = match server_state.operations.reserve([manifest.recovery_claim()]) {
                Ok(reservation) => reservation,
                Err(conflict) => {
                    return HandleOutcome::Sync(Err(format!(
                        "deployment recovery resource is busy: {:?}",
                        conflict.key
                    )));
                }
            };
            let images =
                if crate::adapters::provisioning::recovery::requires_runtime_image_probe(&manifest)
                {
                    recovery_images(dbus).await
                } else {
                    Ok(Vec::new())
                };
            let observations = crate::adapters::provisioning::recovery::probe_manifest_locally(
                &manifest,
                images,
                trusted_state_root,
            )
            .await;
            let result = ProbeDeploymentRecoveryResult {
                deployment_id: manifest.deployment_id,
                manifest_revision: manifest.revision,
                observations,
            };
            HandleOutcome::Sync(serde_json::to_value(result).map_err(|error| error.to_string()))
        }

        RpcMethod::ReconcileDeployment => {
            let request: DeploymentJobRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid reconcile_deployment request: {error}"
                    )));
                }
            };
            let current = match server_state.deployments.snapshot(request.deployment_id) {
                Ok(snapshot) if snapshot.claim == DeploymentClaimState::ReconciliationRequired => {
                    snapshot
                }
                Ok(_) => {
                    return HandleOutcome::Sync(Err(format!(
                        "deployment {} does not require reconciliation",
                        request.deployment_id
                    )));
                }
                Err(error) => return HandleOutcome::Sync(Err(error.to_string())),
            };
            let state = crate::adapters::provisioning::state::FilesystemDeploymentState::new(
                trusted_state_root,
            );
            let manifests = match state.unfinished().await {
                Ok(manifests) => manifests,
                Err(error) => return HandleOutcome::Sync(Err(error.to_string())),
            };
            let manifest_present = manifests
                .iter()
                .any(|manifest| manifest.deployment_id == request.deployment_id);
            log::warn!(
                "[AUDIT] Reconciled deployment {} revision {} against trusted crash state; manifest_present={manifest_present}",
                request.deployment_id,
                current.revision,
            );
            match server_state
                .deployments
                .reconcile(request.deployment_id, manifest_present)
            {
                Ok(snapshot) => HandleOutcome::Sync(
                    serde_json::to_value(snapshot).map_err(|error| error.to_string()),
                ),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::ReleaseUnresolvedDeployment => {
            let request: ReleaseUnresolvedDeploymentRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid release_unresolved_deployment request: {error}"
                    )));
                }
            };
            match server_state
                .deployments
                .release_unresolved(request.deployment_id, request.confirmed)
            {
                Ok(snapshot) => HandleOutcome::Sync(
                    serde_json::to_value(snapshot).map_err(|error| error.to_string()),
                ),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        _ => unreachable!("non-job method routed to job dispatcher"),
    }
}

async fn recovery_images<B: DaemonRuntimeQueries>(
    dbus: &Option<B>,
) -> Result<Vec<ImageEntry>, String> {
    if let Some(dbus) = dbus {
        if dbus.is_available().await {
            match dbus.list_images().await {
                Ok(images) => return Ok(images),
                Err(error) => log::warn!(
                    "Deployment recovery image probe is falling back from D-Bus: {error}"
                ),
            }
        }
    }

    let runner: Arc<dyn crate::adapters::process::CommandRunner> =
        Arc::new(crate::adapters::process::DefaultCommandRunner);
    let cli = crate::adapters::runtime::cli::CliBackend::new(runner);
    RuntimeSource::list_images(&cli)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_job_methods_are_routed_to_this_family() {
        for method in RpcMethod::ALL {
            if method.family() == RpcFamily::Job {
                assert!(matches!(
                    method,
                    RpcMethod::DeploymentStatus
                        | RpcMethod::ResolveDeploymentSubmission
                        | RpcMethod::AcknowledgeDeploymentSubmission
                        | RpcMethod::CancelDeployment
                        | RpcMethod::AcknowledgeDeployment
                        | RpcMethod::ProbeDeploymentRecovery
                        | RpcMethod::ReconcileDeployment
                        | RpcMethod::ReleaseUnresolvedDeployment
                ));
            }
        }
    }
}
