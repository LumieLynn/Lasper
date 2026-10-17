//! Deployment Job-family daemon handlers.
//!
//! This module owns the lifecycle protocol around a deployment record. The
//! deployment executor itself remains in the provisioning application layer;
//! these handlers only inspect or transition daemon-owned job state.

pub(crate) mod server;

use super::server::DaemonServerState;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::provisioning::DeploymentStatePort;
use crate::daemon::dispatch::backend::DaemonRuntimeQueries;
use crate::daemon::dispatch::{decode_params, encode_result, HandlerResult};
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
) -> HandlerResult {
    let JobContext {
        params,
        dbus,
        server_state,
        trusted_state_root,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Job);

    match method {
        RpcMethod::DeploymentStatus => {
            let request: DeploymentJobRequest = decode_params(method, params)?;
            match server_state.deployments.snapshot(request.deployment_id) {
                Ok(snapshot) => encode_result(Some(snapshot)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::ResolveDeploymentSubmission => {
            let request: DeploymentSubmissionRequest = decode_params(method, params)?;
            match server_state
                .deployments
                .resolve_submission(request.request_id)
            {
                Ok(snapshot) => encode_result(Some(snapshot)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::AcknowledgeDeploymentSubmission => {
            let request: DeploymentSubmissionRequest = decode_params(method, params)?;
            match server_state
                .deployments
                .acknowledge_submission(request.request_id)
            {
                Ok(()) => Ok(serde_json::json!({})),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::CancelDeployment => {
            let request: DeploymentJobRequest = decode_params(method, params)?;
            match server_state.deployments.cancel(request.deployment_id) {
                Ok(snapshot) => encode_result(snapshot),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::AcknowledgeDeployment => {
            let request: DeploymentJobRequest = decode_params(method, params)?;
            match server_state.deployments.acknowledge(request.deployment_id) {
                Ok(()) => Ok(serde_json::json!({})),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::ProbeDeploymentRecovery => {
            let request: ProbeDeploymentRecoveryRequest = decode_params(method, params)?;
            let state = crate::adapters::provisioning::state::FilesystemDeploymentState::new(
                trusted_state_root.clone(),
            );
            let manifests = match state.unfinished().await {
                Ok(manifests) => manifests,
                Err(error) => return Err(error.to_string()),
            };
            let manifest = match manifests
                .into_iter()
                .find(|manifest| manifest.deployment_id == request.deployment_id)
            {
                Some(manifest) if manifest.revision == request.expected_revision => manifest,
                Some(manifest) => {
                    return Err(format!(
                        "deployment {} manifest revision changed from {} to {}",
                        request.deployment_id, request.expected_revision, manifest.revision
                    ));
                }
                None => {
                    return Err(format!(
                        "deployment {} crash manifest is missing",
                        request.deployment_id
                    ));
                }
            };
            let _reservation = match server_state.operations.reserve([manifest.recovery_claim()]) {
                Ok(reservation) => reservation,
                Err(conflict) => {
                    return Err(format!(
                        "deployment recovery resource is busy: {:?}",
                        conflict.key
                    ));
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
            encode_result(result)
        }

        RpcMethod::ReconcileDeployment => {
            let request: DeploymentJobRequest = decode_params(method, params)?;
            let current = match server_state.deployments.snapshot(request.deployment_id) {
                Ok(snapshot) if snapshot.claim == DeploymentClaimState::ReconciliationRequired => {
                    snapshot
                }
                Ok(_) => {
                    return Err(format!(
                        "deployment {} does not require reconciliation",
                        request.deployment_id
                    ));
                }
                Err(error) => return Err(error.to_string()),
            };
            let state = crate::adapters::provisioning::state::FilesystemDeploymentState::new(
                trusted_state_root,
            );
            let manifests = match state.unfinished().await {
                Ok(manifests) => manifests,
                Err(error) => return Err(error.to_string()),
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
                Ok(snapshot) => encode_result(snapshot),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::ReleaseUnresolvedDeployment => {
            let request: ReleaseUnresolvedDeploymentRequest = decode_params(method, params)?;
            match server_state
                .deployments
                .release_unresolved(request.deployment_id, request.confirmed)
            {
                Ok(snapshot) => encode_result(snapshot),
                Err(error) => Err(error.to_string()),
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
    let systemd_tools = crate::adapters::runtime::systemd_tools::SystemdToolsBackend::new(runner);
    RuntimeSource::list_images(&systemd_tools)
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
