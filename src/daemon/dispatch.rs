//! Bounded control-channel request pump and typed privileged dispatch.

use super::process_state::{
    shutdown_daemon_resources, OCI_TRANSFER_CANCELLATIONS, SPAWN_EXIT_CODES, SPAWN_PIDS,
    SPAWN_WAIT_ERRORS,
};
use super::protocol::{CliInspectMachineRequest, RpcRequest};
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
use crate::adapters::rootfs::store::{execute_rootfs_operation, RootfsOperation};
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::storage::store::{execute_managed_storage_operation, ManagedStorageOperation};
use crate::adapters::system_operation::{
    execute_cli_image_remove, execute_dbus_system_operation, execute_system_operation,
    SystemOperation,
};
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
    let operation_registry = crate::application::OperationRegistry::new();
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
                        "error":{"code":-32700,"message":format!("Parse error: {}", e)}
                    }),
                )
                .await
                {
                    return Ok(());
                }
                continue;
            }
        };

        let reservation = match daemon_resource_claim(&request) {
            Ok(Some(claim)) => match operation_registry.reserve([claim]) {
                Ok(reservation) => Some(reservation),
                Err(conflict) => {
                    if !send(
                        out_tx,
                        serde_json::json!({
                            "jsonrpc":"2.0","id":request.id,
                            "error":{"code":-32002,"message":format!(
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
                        "error":{"code":-32602,"message":error}
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
                        "error":{"code":-32001,"message":"daemon request limit reached"}
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
                            "jsonrpc":"2.0","id":response_id,"error":{"code":-1,"message":e}
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
    if !matches!(
        request.method.as_str(),
        "system_operation" | "dbus_system_operation" | "image_remove" | "machine_control"
    ) {
        return Ok(None);
    }
    let key = if request.method == "image_remove" {
        let request: ImageRemoveRequest = serde_json::from_value(request.params.clone())
            .map_err(|error| format!("invalid image_remove request: {error}"))?;
        crate::application::ResourceKey::for_image(&request.image)
    } else if request.method == "machine_control" {
        let request: MachineControlRequest = serde_json::from_value(request.params.clone())
            .map_err(|error| format!("invalid machine_control request: {error}"))?;
        crate::application::ResourceKey::for_machine(&request.machine)
    } else {
        let operation: SystemOperation = serde_json::from_value(request.params.clone())
            .map_err(|error| format!("invalid {} request: {error}", request.method))?;
        match operation {
            SystemOperation::Start { machine } => {
                crate::application::ResourceKey::for_machine(&machine)
            }
            SystemOperation::RemoveImage { image } => {
                crate::application::ResourceKey::for_image(&image)
            }
            _ => return Ok(None),
        }
    };
    Ok(Some(crate::application::ResourceClaim::exclusive(key)))
}

pub(super) async fn handle_request<B: DaemonDbusExecutor>(
    request: RpcRequest,
    dbus: &Option<B>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    invoking_uid: u32,
    server_state: Arc<DaemonServerState>,
) -> HandleOutcome {
    let RpcRequest {
        id, method, params, ..
    } = request;
    match method.as_str() {
        "ping" => HandleOutcome::Sync(Ok(serde_json::Value::Null)),

        "close_session" => {
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

        "nspawn_config" => {
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

        "systemd_unit" => {
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

        "nvidia_state" => {
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
                let response = match execute_nvidia_state_operation(operation).await {
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

        "managed_storage" => {
            let operation: ManagedStorageOperation = match serde_json::from_value(params) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid managed_storage request: {error}"
                    )));
                }
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_managed_storage_operation(operation).await {
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

        "rootfs" => {
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

        "assess_tar_runtime" => {
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

        "system_operation" => {
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

        "cli_inspect_machine" => {
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

        "dbus_list_machines" => {
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

        "dbus_list_images" => {
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

        "dbus_system_operation" => {
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

        "machine_control" => {
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

        "image_remove" => {
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

        "dbus_get_properties" => {
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

        "dbus_is_available" => match dbus {
            Some(dbus) => {
                HandleOutcome::Sync(Ok(serde_json::Value::Bool(dbus.is_available().await)))
            }
            None => HandleOutcome::Sync(Ok(serde_json::Value::Bool(false))),
        },

        "wait_command" => {
            let cmd_id = match params["cmd_id"].as_u64() {
                Some(id) => id,
                None => return HandleOutcome::Sync(Err("missing cmd_id".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                loop {
                    let error = SPAWN_WAIT_ERRORS.lock().remove(&cmd_id);
                    if let Some(error) = error {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -1, "message": error},
                        });
                        let line = serde_json::to_string(&response).unwrap();
                        let _ = out_tx.send(line).await;
                        return;
                    }
                    let code = SPAWN_EXIT_CODES.lock().remove(&cmd_id);
                    if let Some(code) = code {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"exit_code": code},
                        });
                        let line = serde_json::to_string(&response).unwrap();
                        let _ = out_tx.send(line).await;
                        return;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            });
            HandleOutcome::Spawned
        }

        "signal_command" => {
            let Some(cmd_id) = params["cmd_id"].as_u64() else {
                return HandleOutcome::Sync(Err("missing cmd_id".into()));
            };
            let signal = match params["signal"].as_str() {
                Some("terminate") => libc::SIGTERM,
                Some("kill") => libc::SIGKILL,
                _ => return HandleOutcome::Sync(Err("unsupported command signal".into())),
            };
            if let Some(cancellation) = OCI_TRANSFER_CANCELLATIONS.lock().get(&cmd_id).cloned() {
                cancellation.request();
                return HandleOutcome::Sync(Ok(serde_json::Value::Null));
            }
            let Some(pid) = SPAWN_PIDS.lock().get(&cmd_id).copied() else {
                return HandleOutcome::Sync(Ok(serde_json::Value::Null));
            };
            let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
            let error = (result != 0).then(std::io::Error::last_os_error);
            if result == 0
                || error.as_ref().and_then(std::io::Error::raw_os_error) == Some(libc::ESRCH)
            {
                HandleOutcome::Sync(Ok(serde_json::Value::Null))
            } else {
                HandleOutcome::Sync(Err(format!(
                    "failed to signal command {cmd_id}: {}",
                    error.expect("failed kill has an OS error")
                )))
            }
        }

        "exit" => {
            shutdown_daemon_resources(&server_state).await;
            std::process::exit(0);
        }

        _ => HandleOutcome::Sync(Err(format!("unknown method: {method}"))),
    }
}

pub(super) fn request_machine_name(params: &serde_json::Value) -> Result<MachineName, String> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| "missing name".to_string())?;
    MachineName::try_from(name).map_err(|error| error.to_string())
}
