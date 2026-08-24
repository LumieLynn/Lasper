//! Command-family daemon handlers.
//!
//! These operations have one request/response contract. Some implementations
//! perform external I/O in a spawned task, but they do not expose accepted
//! job state; long-running stateful work belongs to `jobs::server`.

use super::super::protocol::{error_code, RpcFamily, RpcMethod};
use super::super::server::DaemonServerState;
use super::handler::{DaemonDbusExecutor, HandleOutcome};
use crate::adapters::config::store::{execute_nspawn_config_operation, NspawnConfigOperation};
use crate::adapters::config::systemd_unit::{execute_systemd_unit_operation, SystemdUnitOperation};
use crate::adapters::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation,
};
use crate::adapters::provisioning::state::{
    execute_deployment_state_operation, DeploymentStateOperation,
};
use crate::adapters::rootfs::store::{execute_rootfs_operation, RootfsOperation};
use crate::adapters::system_operation::{
    execute_cli_image_remove, execute_system_operation, SystemOperation,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::{
    ImageControlOutcome, ImageRemoveRequest, ImageRemoveTransport,
};
use crate::application::machine_lifecycle::{
    MachineControlOutcome, MachineControlRequest, MachineControlTransport,
};
use serde_json::Value;
use std::sync::Arc;

pub(super) struct CommandContext<'a, B> {
    pub(super) id: u64,
    pub(super) params: Value,
    pub(super) dbus: &'a Option<B>,
    pub(super) out_tx: &'a tokio::sync::mpsc::Sender<String>,
    pub(super) invoking_uid: u32,
    pub(super) server_state: Arc<DaemonServerState>,
    pub(super) trusted_state_root: TrustedStateRoot,
}

pub(super) async fn handle<B: DaemonDbusExecutor>(
    method: RpcMethod,
    context: CommandContext<'_, B>,
) -> HandleOutcome {
    let CommandContext {
        id,
        params,
        dbus,
        out_tx,
        invoking_uid,
        server_state,
        trusted_state_root,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Command);

    match method {
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
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":error.to_string(),
                    }}),
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
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":error.to_string(),
                    }}),
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
                        Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":error_code::INTERNAL_ERROR,
                            "message":error.to_string(),
                        }}),
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
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":error.to_string(),
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
                Ok(()) => HandleOutcome::Sync(Ok(Value::Null)),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::MachineControl => {
            let request: MachineControlRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid machine_control request: {error}"
                    )));
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
                    )));
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
                        Err(error) => {
                            crate::adapters::lifecycle::error::map_image_control_error(error)
                        }
                    },
                    None => ImageControlOutcome::NotAttempted {
                        reason: "DBus backend is unavailable".into(),
                    },
                },
                ImageRemoveTransport::Cli => execute_cli_image_remove(request.image).await,
            };
            HandleOutcome::Sync(serde_json::to_value(outcome).map_err(|error| error.to_string()))
        }

        RpcMethod::Exit => {
            super::super::server::shutdown_daemon_resources(&server_state).await;
            std::process::exit(0);
        }

        _ => unreachable!("non-command method routed to command dispatcher"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_inventory_excludes_query_job_and_session_methods() {
        for method in RpcMethod::ALL {
            if method.family() == RpcFamily::Command {
                assert!(!matches!(
                    method,
                    RpcMethod::Ping
                        | RpcMethod::CloseSession
                        | RpcMethod::DeploymentStatus
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
