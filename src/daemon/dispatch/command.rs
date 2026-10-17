//! Command-family daemon handlers.
//!
//! Each handler awaits its operation and returns one result to the request
//! pump. Accepted, independently running deployments belong to `jobs::server`.

use crate::adapters::config::store::{execute_nspawn_config_operation, NspawnConfigOperation};
use crate::adapters::config::systemd_unit::{execute_systemd_unit_operation, SystemdUnitOperation};
use crate::adapters::lifecycle::error::map_system_operation_image_error;
use crate::adapters::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation,
};
use crate::adapters::provisioning::state::{
    execute_deployment_state_operation, DeploymentStateOperation,
};
use crate::adapters::system_operation::{
    execute_system_operation, execute_systemd_tools_image_remove, SystemOperation,
};
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::{
    ImageControlOutcome, ImageRemoveRequest, ImageRemoveTransport,
};
use crate::application::machine_lifecycle::{
    validate_nspawn_runtime_entry, MachineControlOutcome, MachineControlTransport,
    MachineRuntimeControlRequest, MachineStartPreparationRequest, NspawnLaunchRequest,
    NspawnUnitControlRequest,
};
use crate::daemon::dispatch::backend::{DaemonRuntimeQueries, DaemonSystemExecutor};
use crate::daemon::dispatch::{decode_params, encode_result, HandlerResult};
use crate::daemon::server::DaemonServerState;
use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use crate::ipc::protocol::systemd_unit as systemd_unit_wire;
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde_json::Value;
use std::sync::Arc;

pub(super) struct CommandContext<'a, B> {
    pub(super) params: Value,
    pub(super) dbus: &'a Option<B>,
    pub(super) invoking_uid: u32,
    pub(super) server_state: Arc<DaemonServerState>,
    pub(super) trusted_state_root: TrustedStateRoot,
}

pub(super) async fn handle<B: DaemonRuntimeQueries + DaemonSystemExecutor>(
    method: RpcMethod,
    context: CommandContext<'_, B>,
) -> HandlerResult {
    let CommandContext {
        params,
        dbus,
        invoking_uid,
        server_state,
        trusted_state_root,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Command);

    match method {
        RpcMethod::Configuration => {
            let result = match serde_json::from_value(params) {
                Ok(operation) => crate::adapters::config::editor::execute(operation).await,
                Err(error) => {
                    Err(crate::application::configuration::ConfigurationError::invalid_input(error))
                }
            };
            encode_result(result)
        }

        RpcMethod::NspawnConfig => {
            let operation: NspawnConfigOperation = decode_params(method, params)?;
            let result = execute_nspawn_config_operation(operation, invoking_uid)
                .await
                .map_err(|error| error.to_string())?;
            encode_result(result)
        }

        RpcMethod::SystemdUnit => {
            let wire_operation: systemd_unit_wire::SystemdUnitOperation =
                decode_params(method, params)?;
            let operation = SystemdUnitOperation::from(wire_operation);
            let result = execute_systemd_unit_operation(operation)
                .await
                .map_err(|error| error.to_string())?;
            encode_result(systemd_unit_wire::SystemdUnitResult::from(result))
        }

        RpcMethod::NvidiaState => {
            let operation: NvidiaStateOperation = decode_params(method, params)?;
            let result = execute_nvidia_state_operation(operation, trusted_state_root)
                .await
                .map_err(|error| error.to_string())?;
            encode_result(result)
        }

        RpcMethod::DeploymentState => {
            let operation: DeploymentStateOperation = decode_params(method, params)?;
            let result = execute_deployment_state_operation(operation, trusted_state_root)
                .await
                .unwrap_or_else(
                    crate::adapters::provisioning::state::DeploymentStateResult::failure,
                );
            encode_result(result)
        }

        RpcMethod::SystemOperation => {
            let request: crate::ipc::protocol::system::SystemOperationRequest =
                decode_params(method, params)?;
            let operation = SystemOperation::from(request.operation);
            let result = match request.transport {
                MachineControlTransport::Dbus => match dbus.as_ref() {
                    Some(dbus) if dbus.is_available().await => {
                        dbus.system_operation(operation).await
                    }
                    _ => execute_system_operation(operation).await,
                },
                MachineControlTransport::SystemdTools => execute_system_operation(operation).await,
            };
            match result {
                Ok(()) => Ok(Value::Null),
                Err(error) => Err(error.to_string()),
            }
        }

        RpcMethod::PrepareMachineStart => {
            let request: MachineStartPreparationRequest = decode_params(method, params)?;
            let host = crate::adapters::lifecycle::machine::DirectStartPreparation::new(
                trusted_state_root,
                request.nvidia_cdi_source,
                false,
            );
            let reload = async {
                let operation = SystemOperation::ReloadDaemon;
                let result = match request.transport {
                    MachineControlTransport::Dbus => match dbus.as_ref() {
                        Some(dbus) if dbus.is_available().await => {
                            dbus.system_operation(operation).await
                        }
                        _ => execute_system_operation(operation).await,
                    },
                    MachineControlTransport::SystemdTools => {
                        execute_system_operation(operation).await
                    }
                };
                result.map_err(crate::adapters::error::NspawnError::from)
            };
            encode_result(host.prepare(&request.machine, reload).await)
        }

        RpcMethod::NspawnLaunch => {
            let request: NspawnLaunchRequest = decode_params(method, params)?;
            if !request.validates_same_name_route() {
                return Err(
                    "invalid nspawn_launch request: image and machine names must match".into(),
                );
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
            encode_result(outcome)
        }

        RpcMethod::MachineRuntimeControl => {
            let request: MachineRuntimeControlRequest = decode_params(method, params)?;
            if let Err(outcome) =
                validate_runtime_target(&request.machine, request.transport, dbus).await
            {
                return encode_result(outcome);
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
            encode_result(outcome)
        }

        RpcMethod::NspawnUnitControl => {
            let request: NspawnUnitControlRequest = decode_params(method, params)?;
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
            encode_result(outcome)
        }

        RpcMethod::ImageRemove => {
            let request: ImageRemoveRequest = decode_params(method, params)?;
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
            encode_result(outcome)
        }

        RpcMethod::Exit => {
            server_state.shutdown.begin();
            Ok(Value::Null)
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
