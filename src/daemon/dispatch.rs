//! Bounded control-channel request pump and typed privileged dispatch.

use super::deployment_protocol::{
    DeploymentJobRequest, DeploymentSubmissionRequest, ProbeDeploymentRecoveryRequest,
    ProbeDeploymentRecoveryResult, ReleaseUnresolvedDeploymentRequest,
};
use super::process_state::shutdown_daemon_resources;
use super::protocol::{error_code, CliInspectMachineRequest, RpcMethod, RpcRequest};
use super::session_protocol::CloseSessionParams;
use super::session_server::DaemonServerState;
use super::transport::{read_bounded_line, MAX_RPC_FRAME_BYTES};
use crate::adapters::config::store::{execute_nspawn_config_operation, NspawnConfigOperation};
use crate::adapters::config::systemd_unit::{execute_systemd_unit_operation, SystemdUnitOperation};
use crate::adapters::lifecycle::error::{map_image_control_error, map_machine_control_error};
use crate::adapters::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation,
};
use crate::adapters::provisioning::engine::image_operation::inspect_tar_runtime;
use crate::adapters::provisioning::state::{
    execute_deployment_state_operation, DeploymentStateOperation,
};
use crate::adapters::rootfs::store::{execute_rootfs_operation, RootfsOperation};
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{
    execute_cli_image_remove, execute_dbus_system_operation, execute_system_operation,
    SystemOperation,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::{
    ImageControlOutcome, ImageRemoveRequest, ImageRemoveTransport,
};
use crate::application::machine_lifecycle::{
    MachineAction, MachineControlOutcome, MachineControlRequest, MachineControlTransport,
};
use crate::domain::secret::zeroize_string;
use crate::nspawn::models::{ContainerEntry, ImageEntry, MachineName, MachineProperties};
use std::sync::Arc;

const MAX_RPC_IN_FLIGHT: usize = 64;

pub(super) enum HandleOutcome {
    Spawned,
    Sync(Result<serde_json::Value, String>),
}

pub(super) async fn initialize_dbus_backend(
    enabled: bool,
) -> Option<crate::adapters::runtime::dbus::DbusBackend> {
    if !enabled {
        return None;
    }

    let dbus = crate::adapters::runtime::dbus::DbusBackend::new();
    RuntimeSource::is_available(&dbus).await.then_some(dbus)
}

/// The D-Bus surface used by the RPC dispatcher.
///
/// This is intentionally private to the daemon for now. It keeps dispatch
/// tests independent from a live system bus without pretending that the
/// application already has a general host capability layer.
#[async_trait::async_trait]
pub(super) trait DaemonDbusExecutor: Send + Sync {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>>;
    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>>;
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()>;
    async fn machine_control(
        &self,
        machine: MachineName,
        action: MachineAction,
    ) -> MachineControlOutcome {
        let operation = match action {
            MachineAction::Start => SystemOperation::Start { machine },
            MachineAction::Terminate => SystemOperation::Terminate { machine },
            MachineAction::Poweroff => SystemOperation::Poweroff { machine },
            MachineAction::Reboot => SystemOperation::Reboot { machine },
            MachineAction::Enable => SystemOperation::Enable { machine },
            MachineAction::Disable => SystemOperation::Disable { machine },
            MachineAction::Kill { signal } => SystemOperation::Kill { machine, signal },
        };
        match self.system_operation(operation).await {
            Ok(()) => MachineControlOutcome::Succeeded,
            Err(error) => map_machine_control_error(error),
        }
    }
    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties>;
    async fn is_available(&self) -> bool;
}

#[async_trait::async_trait]
impl DaemonDbusExecutor for crate::adapters::runtime::dbus::DbusBackend {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>> {
        RuntimeSource::list_machines(self).await
    }

    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>> {
        RuntimeSource::list_images(self).await
    }

    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()> {
        execute_dbus_system_operation(self, operation).await
    }

    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties> {
        RuntimeSource::get_properties(self, name).await
    }

    async fn is_available(&self) -> bool {
        RuntimeSource::is_available(self).await
    }
}

pub(super) async fn run_rpc_request_pump<R, B>(
    reader: &mut R,
    dbus: &Option<B>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    invoking_uid: u32,
    server_state: Arc<DaemonServerState>,
    trusted_state_root: TrustedStateRoot,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    B: DaemonDbusExecutor + Clone + 'static,
{
    async fn send(out_tx: &tokio::sync::mpsc::Sender<String>, json: serde_json::Value) -> bool {
        let line = serde_json::to_string(&json).expect("JSON-RPC values are serializable");
        out_tx.send(line).await.is_ok()
    }

    let request_slots = Arc::new(tokio::sync::Semaphore::new(MAX_RPC_IN_FLIGHT));
    loop {
        let Some(mut line) = read_bounded_line(reader, MAX_RPC_FRAME_BYTES).await? else {
            return Ok(());
        };
        if line.trim().is_empty() {
            zeroize_string(&mut line);
            continue;
        }

        let parsed = serde_json::from_str(&line);
        zeroize_string(&mut line);
        let request: RpcRequest = match parsed {
            Ok(req) => req,
            Err(e) => {
                if !send(
                    out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":null,
                        "error":{"code":error_code::PARSE_ERROR,"message":format!("Parse error: {}", e)}
                    }),
                )
                .await
                {
                    return Ok(());
                }
                continue;
            }
        };

        let method = match request.method_kind() {
            Ok(method) => method,
            Err(error) => {
                if !send(
                    out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":request.id,
                        "error":{"code":error.code,"message":error.message}
                    }),
                )
                .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        log::trace!(
            "Daemon RPC: accepted {} method in {} family",
            method.wire_name(),
            method.family().as_str()
        );

        let reservation = match daemon_resource_claim(&request) {
            Ok(Some(claim)) => match server_state.operations.reserve([claim]) {
                Ok(reservation) => Some(reservation),
                Err(conflict) => {
                    if !send(
                        out_tx,
                        serde_json::json!({
                            "jsonrpc":"2.0","id":request.id,
                            "error":{"code":error_code::RESOURCE_BUSY,"message":format!(
                                "resource is busy: {:?}", conflict.key
                            )}
                        }),
                    )
                    .await
                    {
                        return Ok(());
                    }
                    continue;
                }
            },
            Ok(None) => None,
            Err(error) => {
                if !send(
                    out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":request.id,
                        "error":{"code":error_code::INVALID_PARAMS,"message":error}
                    }),
                )
                .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        let permit = match request_slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if !send(
                    out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":request.id,
                        "error":{"code":error_code::REQUEST_LIMIT,"message":"daemon request limit reached"}
                    }),
                )
                .await
                {
                    return Ok(());
                }
                continue;
            }
        };

        let dbus = dbus.clone();
        let out_tx = out_tx.clone();
        let server_state = Arc::clone(&server_state);
        let trusted_state_root = trusted_state_root.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _reservation = reservation;
            let response_id = request.id;
            match handle_request(
                request,
                &dbus,
                &out_tx,
                invoking_uid,
                Arc::clone(&server_state),
                trusted_state_root,
            )
            .await
            {
                HandleOutcome::Spawned => {}
                HandleOutcome::Sync(Ok(result)) => {
                    let _ = send(
                        &out_tx,
                        serde_json::json!({
                            "jsonrpc":"2.0","id":response_id,"result":result
                        }),
                    )
                    .await;
                }
                HandleOutcome::Sync(Err(e)) => {
                    let _ = send(
                        &out_tx,
                        serde_json::json!({
                            "jsonrpc":"2.0","id":response_id,"error":{"code":error_code::INTERNAL_ERROR,"message":e}
                        }),
                    )
                    .await;
                }
            }
        });
    }
}

pub(super) fn daemon_resource_claim(
    request: &RpcRequest,
) -> Result<Option<crate::application::ResourceClaim>, String> {
    let method = request.method_kind().map_err(|error| error.message)?;
    let key = match method {
        RpcMethod::ImageRemove => {
            let request: ImageRemoveRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid image_remove request: {error}"))?;
            crate::application::ResourceKey::for_image(&request.image)
        }
        RpcMethod::MachineControl => {
            let request: MachineControlRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid machine_control request: {error}"))?;
            crate::application::ResourceKey::for_machine(&request.machine)
        }
        RpcMethod::SystemOperation | RpcMethod::DbusSystemOperation => {
            let operation: SystemOperation = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid {} request: {error}", method.wire_name()))?;
            match operation {
                SystemOperation::Start { machine } => {
                    crate::application::ResourceKey::for_machine(&machine)
                }
                SystemOperation::RemoveImage { image } => {
                    crate::application::ResourceKey::for_image(&image)
                }
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(crate::application::ResourceClaim::exclusive(key)))
}

pub(super) async fn handle_request<B: DaemonDbusExecutor>(
    request: RpcRequest,
    dbus: &Option<B>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    invoking_uid: u32,
    server_state: Arc<DaemonServerState>,
    trusted_state_root: TrustedStateRoot,
) -> HandleOutcome {
    let RpcRequest {
        id, method, params, ..
    } = request;
    let method = match RpcMethod::parse(&method) {
        Some(method) => method,
        None => return HandleOutcome::Sync(Err(format!("unknown method: {method}"))),
    };
    match method {
        RpcMethod::Ping => HandleOutcome::Sync(Ok(serde_json::Value::Null)),

        RpcMethod::CloseSession => {
            let params: CloseSessionParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid close_session request: {error}"
                    )))
                }
            };
            HandleOutcome::Sync(
                server_state
                    .close_and_escalate(params.session_id)
                    .map(|()| serde_json::Value::Null)
                    .map_err(|error| error.to_string()),
            )
        }

        RpcMethod::NspawnConfig => {
            let operation: NspawnConfigOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid nspawn_config request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_nspawn_config_operation(operation, invoking_uid).await
                {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::SystemdUnit => {
            let operation: SystemdUnitOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid systemd_unit request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_systemd_unit_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::NvidiaState => {
            let operation: NvidiaStateOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid nvidia_state request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response =
                    match execute_nvidia_state_operation(operation, trusted_state_root).await {
                        Ok(result) => {
                            serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                        }
                        Err(error) => {
                            serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                                "code":-1,
                                "message":error.to_string(),
                            }})
                        }
                    };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::DeploymentState => {
            let operation: DeploymentStateOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid deployment_state request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = execute_deployment_state_operation(operation, trusted_state_root)
                    .await
                    .unwrap_or_else(
                        crate::adapters::provisioning::state::DeploymentStateResult::failure,
                    );
                let response = serde_json::json!({"jsonrpc":"2.0","id":id,"result":result});
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

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
                    HandleOutcome::Sync(Ok(serde_json::Value::Null))
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
                    HandleOutcome::Sync(Ok(serde_json::Value::Null))
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
            let manifests =
                match crate::application::provisioning::DeploymentStatePort::unfinished(&state)
                    .await
                {
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
                Ok(snapshot)
                    if snapshot.claim
                        == super::deployment_protocol::DeploymentClaimState::ReconciliationRequired =>
                {
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
            let manifests =
                match crate::application::provisioning::DeploymentStatePort::unfinished(&state)
                    .await
                {
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

        RpcMethod::Rootfs => {
            let operation: RootfsOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!("invalid rootfs request: {error}")));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_rootfs_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::AssessTarRuntime => {
            if params != serde_json::json!({}) {
                return HandleOutcome::Sync(Err(
                    "assess_tar_runtime does not accept parameters".into()
                ));
            }
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(inspect_tar_runtime).await;
                let response = match result {
                    Ok(Ok(assessment)) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":assessment})
                    }
                    Ok(Err(error)) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":error.to_string(),
                    }}),
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":format!("tar runtime inspection task failed: {error}"),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::SystemOperation => {
            let operation: SystemOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid system_operation request: {error}"
                    )));
                }
            };
            match execute_system_operation(operation).await {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::CliInspectMachine => {
            let inspection: CliInspectMachineRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid cli_inspect_machine request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match crate::adapters::runtime::cli::get_properties_with_runner(
                    inspection.machine.as_str(),
                    &crate::adapters::process::DefaultCommandRunner,
                )
                .await
                {
                    Ok(properties) => match serde_json::to_value(properties) {
                        Ok(result) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                        Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }}),
                    },
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":error.to_string(),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::DbusListMachines => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_machines().await {
                Ok(machines) => match serde_json::to_value(machines) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        RpcMethod::DbusListImages => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_images().await {
                Ok(images) => match serde_json::to_value(images) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        RpcMethod::DbusSystemOperation => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let operation: SystemOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid dbus_system_operation request: {error}"
                    )));
                }
            };
            match dbus.system_operation(operation).await {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        RpcMethod::MachineControl => {
            let request: MachineControlRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid machine_control request: {error}"
                    )))
                }
            };
            let outcome = match request.transport {
                MachineControlTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) => dbus.machine_control(request.machine, request.action).await,
                    None => MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    },
                },
                MachineControlTransport::Cli => {
                    crate::adapters::lifecycle::machine::execute_cli_machine_control(
                        request.machine,
                        request.action,
                    )
                    .await
                }
            };
            HandleOutcome::Sync(serde_json::to_value(outcome).map_err(|error| error.to_string()))
        }

        RpcMethod::ImageRemove => {
            let request: ImageRemoveRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid image_remove request: {error}"
                    )))
                }
            };
            let outcome = match request.transport {
                ImageRemoveTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) => match dbus
                        .system_operation(SystemOperation::RemoveImage {
                            image: request.image,
                        })
                        .await
                    {
                        Ok(()) => ImageControlOutcome::Removed,
                        Err(error) => map_image_control_error(error),
                    },
                    None => ImageControlOutcome::NotAttempted {
                        reason: "DBus backend is unavailable".into(),
                    },
                },
                ImageRemoveTransport::Cli => execute_cli_image_remove(request.image).await,
            };
            HandleOutcome::Sync(serde_json::to_value(outcome).map_err(|error| error.to_string()))
        }

        RpcMethod::DbusGetProperties => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let name = match request_machine_name(&params) {
                Ok(name) => name,
                Err(error) => return HandleOutcome::Sync(Err(error)),
            };
            match dbus.get_properties(name.as_str()).await {
                Ok(props) => match serde_json::to_value(props) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        RpcMethod::DbusIsAvailable => match dbus {
            Some(dbus) => {
                HandleOutcome::Sync(Ok(serde_json::Value::Bool(dbus.is_available().await)))
            }
            None => HandleOutcome::Sync(Ok(serde_json::Value::Bool(false))),
        },

        RpcMethod::Exit => {
            shutdown_daemon_resources(&server_state).await;
            std::process::exit(0);
        }
    }
}

async fn recovery_images<B: DaemonDbusExecutor>(
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

pub(super) fn request_machine_name(params: &serde_json::Value) -> Result<MachineName, String> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| "missing name".to_string())?;
    MachineName::try_from(name).map_err(|error| error.to_string())
}
