//! Command-family daemon handlers.
//!
//! These operations have one request/response contract. Some implementations
//! perform external I/O in a spawned task, but they do not expose accepted
//! job state; long-running stateful work belongs to `jobs::server`.

use super::super::server::DaemonServerState;
use super::handler::{DaemonRuntimeQueries, DaemonSystemExecutor, HandleOutcome};
use crate::adapters::config::store::{execute_nspawn_config_operation, NspawnConfigOperation};
use crate::adapters::config::systemd_unit::{execute_systemd_unit_operation, SystemdUnitOperation};
use crate::adapters::lifecycle::error::map_system_operation_image_error;
use crate::adapters::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation,
};
use crate::adapters::provisioning::state::{
    execute_deployment_state_operation, DeploymentStateOperation,
};
use crate::adapters::rootfs::store::{execute_rootfs_operation, RootfsOperation};
use crate::adapters::system_operation::{
    execute_system_operation, execute_systemd_tools_image_remove, SystemOperation,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::{
    ImageControlOutcome, ImageRemoveRequest, ImageRemoveTransport,
};
use crate::application::machine_lifecycle::{
    validate_nspawn_runtime_entry, MachineControlOutcome, MachineControlTransport,
    MachineRuntimeControlRequest, NspawnLaunchRequest, NspawnUnitControlRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use crate::ipc::protocol::rootfs as rootfs_wire;
use crate::ipc::protocol::systemd_unit as systemd_unit_wire;
use crate::ipc::protocol::{error_code, RpcFamily, RpcMethod};
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

pub(super) async fn handle<B: DaemonRuntimeQueries + DaemonSystemExecutor>(
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
            let wire_operation: systemd_unit_wire::SystemdUnitOperation =
                match serde_json::from_value(params) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid systemd_unit request: {error}"
                        )));
                    }
                };
            let operation = SystemdUnitOperation::from(wire_operation);
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_systemd_unit_operation(operation).await {
                    Ok(result) => {
                        let result = systemd_unit_wire::SystemdUnitResult::from(result);
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
            let wire_operation: rootfs_wire::RootfsOperation = match serde_json::from_value(params)
            {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!("invalid rootfs request: {error}")));
                }
            };
            let operation = match RootfsOperation::try_from(wire_operation) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!("invalid rootfs request: {error}")));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_rootfs_operation(operation).await {
                    Ok(result) => {
                        let result = rootfs_wire::RootfsResult::from(result);
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
            let wire_operation: crate::ipc::protocol::system::SystemOperation =
                match serde_json::from_value(params) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid system_operation request: {error}"
                        )));
                    }
                };
            let operation = SystemOperation::from(wire_operation);
            match execute_system_operation(operation).await {
                Ok(()) => HandleOutcome::Sync(Ok(Value::Null)),
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::NspawnLaunch => {
            let request: NspawnLaunchRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid nspawn_launch request: {error}"
                    )));
                }
            };
            if !request.validates_same_name_route() {
                return HandleOutcome::Sync(Err(
                    "invalid nspawn_launch request: image and machine names must match".into(),
                ));
            }
            let outcome = match request.transport {
                MachineControlTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) => dbus.nspawn_launch(request.image, request.machine).await,
                    None => MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    },
                },
                MachineControlTransport::SystemdTools => {
                    crate::adapters::lifecycle::machine::execute_systemd_tools_nspawn_launch(
                        request.image,
                        request.machine,
                    )
                    .await
                }
            };
            HandleOutcome::Sync(serde_json::to_value(outcome).map_err(|error| error.to_string()))
        }

        RpcMethod::MachineRuntimeControl => {
            let request: MachineRuntimeControlRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid machine_runtime_control request: {error}"
                    )));
                }
            };
            if let Err(outcome) =
                validate_runtime_target(&request.machine, request.transport, dbus).await
            {
                return HandleOutcome::Sync(
                    serde_json::to_value(outcome).map_err(|error| error.to_string()),
                );
            }
            let outcome = match request.transport {
                MachineControlTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) => {
                        dbus.machine_runtime_control(request.machine, request.action)
                            .await
                    }
                    None => MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    },
                },
                MachineControlTransport::SystemdTools => {
                    crate::adapters::lifecycle::machine::execute_systemd_tools_machine_runtime(
                        request.machine,
                        request.action,
                    )
                    .await
                }
            };
            HandleOutcome::Sync(serde_json::to_value(outcome).map_err(|error| error.to_string()))
        }

        RpcMethod::NspawnUnitControl => {
            let request: NspawnUnitControlRequest = match serde_json::from_value(params) {
                Ok(request) => request,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid nspawn_unit_control request: {error}"
                    )));
                }
            };
            let outcome = match request.transport {
                MachineControlTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) => {
                        dbus.nspawn_unit_control(request.machine, request.action)
                            .await
                    }
                    None => MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    },
                },
                MachineControlTransport::SystemdTools => {
                    crate::adapters::lifecycle::machine::execute_systemd_tools_nspawn_unit(
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
                        Err(error) => map_system_operation_image_error(error),
                    },
                    None => ImageControlOutcome::NotAttempted {
                        reason: "DBus backend is unavailable".into(),
                    },
                },
                ImageRemoveTransport::SystemdTools => {
                    match execute_systemd_tools_image_remove(request.image).await {
                        Ok(()) => ImageControlOutcome::Removed,
                        Err(error) => map_system_operation_image_error(error),
                    }
                }
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

/// Re-read the machined registration at the privileged boundary before a
/// runtime mutation. A client-provided machine name is not proof that the
/// target is an nspawn container; VM and foreign-provider registrations must
/// remain visible but read-only even if a caller bypasses the TUI.
async fn validate_runtime_target<B: DaemonRuntimeQueries>(
    machine: &MachineName,
    transport: MachineControlTransport,
    dbus: &Option<B>,
) -> Result<(), MachineControlOutcome> {
    let entries = match transport {
        MachineControlTransport::Dbus => {
            let Some(dbus) = dbus.as_ref() else {
                return Err(MachineControlOutcome::NotAttempted {
                    reason: "D-Bus backend is unavailable".into(),
                });
            };
            dbus.list_machines()
                .await
                .map_err(|error| MachineControlOutcome::NotAttempted {
                    reason: format!("could not verify machine registration: {error}"),
                })?
        }
        MachineControlTransport::SystemdTools => {
            crate::adapters::runtime::state::list_machines_at(crate::paths::runtime_machines_dir())
                .await
                .map_err(|error| MachineControlOutcome::NotAttempted {
                    reason: format!("could not verify machine registration: {error}"),
                })?
        }
    };

    validate_runtime_entries(&entries, machine)
}

fn validate_runtime_entries(
    entries: &[MachineEntry],
    machine: &MachineName,
) -> Result<(), MachineControlOutcome> {
    let Some(entry) = entries.iter().find(|entry| entry.name == machine.as_str()) else {
        return Err(MachineControlOutcome::Rejected {
            rejection: crate::application::machine_lifecycle::MachineRejection::NotFound,
            reason: format!("machine '{}' is not registered with machined", machine),
        });
    };

    validate_nspawn_runtime_entry(entry).map_err(|rejection| MachineControlOutcome::Rejected {
        reason: format!(
            "machine '{}' cannot be controlled by nspawn route: {rejection}",
            machine
        ),
        rejection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, class: &str, service: &str) -> MachineEntry {
        MachineEntry {
            name: name.into(),
            class: class.into(),
            service: service.into(),
            state: crate::domain::runtime::MachineState::Running,
            addresses: Default::default(),
        }
    }

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

    #[test]
    fn runtime_validation_rejects_foreign_registrations_without_host_calls() {
        let target = MachineName::new("guest").unwrap();
        let vm = machine("guest", "vm", "systemd-vmspawn");
        let result = validate_runtime_entries(&[vm], &target);
        assert!(matches!(
            result,
            Err(MachineControlOutcome::Rejected {
                rejection: crate::application::machine_lifecycle::MachineRejection::Unsupported,
                ..
            })
        ));
    }

    #[test]
    fn runtime_validation_accepts_only_the_exact_nspawn_registration() {
        let target = MachineName::new("guest").unwrap();
        let nspawn = machine("guest", "container", "systemd-nspawn");
        assert!(validate_runtime_entries(&[nspawn], &target).is_ok());
    }

    #[test]
    fn runtime_validation_distinguishes_an_unregistered_machine() {
        let target = MachineName::new("missing").unwrap();
        let result =
            validate_runtime_entries(&[machine("other", "container", "systemd-nspawn")], &target);
        assert!(matches!(
            result,
            Err(MachineControlOutcome::Rejected {
                rejection: crate::application::machine_lifecycle::MachineRejection::NotFound,
                ..
            })
        ));
    }
}
