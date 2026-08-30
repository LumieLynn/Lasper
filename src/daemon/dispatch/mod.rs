//! Bounded control-channel request pump and typed privileged dispatch.

mod command;
pub(crate) mod handler;
pub(crate) mod query;

use self::handler::{DaemonRuntimeQueries, DaemonSystemExecutor, HandleOutcome};
use super::server::DaemonServerState;
use crate::adapters::system_operation::SystemOperation;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::image_lifecycle::ImageRemoveRequest;
use crate::application::machine_lifecycle::{
    MachineRuntimeControlRequest, NspawnLaunchRequest, NspawnUnitControlRequest,
};
use crate::domain::secret::zeroize_string;
use crate::ipc::protocol::{error_code, RpcFamily, RpcMethod, RpcRequest};
use crate::ipc::transport::{read_bounded_line, MAX_RPC_FRAME_BYTES};
use std::sync::Arc;

const MAX_RPC_IN_FLIGHT: usize = 64;

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
    B: DaemonRuntimeQueries + DaemonSystemExecutor + Clone + 'static,
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
        RpcMethod::NspawnLaunch => {
            let request: NspawnLaunchRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid nspawn_launch request: {error}"))?;
            if !request.validates_same_name_route() {
                return Err(
                    "invalid nspawn_launch request: image and machine names must match".into(),
                );
            }
            crate::application::ResourceKey::for_image(&request.image)
        }
        RpcMethod::MachineRuntimeControl => {
            let request: MachineRuntimeControlRequest =
                serde_json::from_value(request.params.clone())
                    .map_err(|error| format!("invalid machine_runtime_control request: {error}"))?;
            crate::application::ResourceKey::for_machine(&request.machine)
        }
        RpcMethod::NspawnUnitControl => {
            let request: NspawnUnitControlRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid nspawn_unit_control request: {error}"))?;
            crate::application::ResourceKey::for_machine(&request.machine)
        }
        RpcMethod::SystemOperation => {
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

pub(super) async fn handle_request<B: DaemonRuntimeQueries + DaemonSystemExecutor>(
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
    if method.family() == RpcFamily::Query {
        return self::query::handle(method, id, params, dbus, out_tx).await;
    }
    if method.family() == RpcFamily::Command {
        return self::command::handle(
            method,
            self::command::CommandContext {
                id,
                params,
                dbus,
                out_tx,
                invoking_uid,
                server_state,
                trusted_state_root,
            },
        )
        .await;
    }
    if method.family() == RpcFamily::Job {
        return super::jobs::handle(
            method,
            super::jobs::JobContext {
                params,
                dbus,
                server_state,
                trusted_state_root,
            },
        )
        .await;
    }
    if method.family() == RpcFamily::Session {
        return super::sessions::handle(
            method,
            super::sessions::SessionContext {
                params,
                server_state,
            },
        )
        .await;
    }
    unreachable!("unhandled RPC family escaped dispatcher")
}
