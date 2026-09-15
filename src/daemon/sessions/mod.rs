//! Session-family JSON-RPC handlers.
//!
//! Process spawning and session tracking live in `sessions::server`; this
//! module owns only the typed control-plane transition exposed to the RPC
//! dispatcher.

pub(crate) mod server;

use super::dispatch::handler::HandleOutcome;
use super::server::DaemonServerState;
use crate::ipc::protocol::session::{
    CloseSessionParams, PrepareWaylandParams, PrepareWaylandResponse, ProbeX11ProjectionParams,
    ProbeX11ProjectionResponse,
};
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct SessionContext {
    pub(super) params: Value,
    pub(super) server_state: Arc<DaemonServerState>,
    pub(super) machine: crate::adapters::session::MachineSessionTransport,
    pub(super) invoking_uid: u32,
}

pub(crate) async fn handle(method: RpcMethod, context: SessionContext) -> HandleOutcome {
    let SessionContext {
        params,
        server_state,
        machine,
        invoking_uid,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Session);

    match method {
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
                    .map(|()| Value::Null)
                    .map_err(|error| error.to_string()),
            )
        }
        RpcMethod::PrepareWayland => {
            let params: PrepareWaylandParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid prepare_wayland request: {error}"
                    )))
                }
            };
            let resolver = crate::adapters::session::WaylandSessionResolver::for_authorized_uid(
                machine,
                crate::adapters::config::NspawnConfigStore::direct(),
                invoking_uid,
            );
            let result = resolver
                .prepare(crate::application::sessions::WaylandPreparationRequest {
                    probe_id: crate::domain::session::SessionId::new(params.probe_id.get())
                        .expect("wire session id is non-zero"),
                    target: crate::application::sessions::ShellTarget::new(
                        params.machine,
                        params.user,
                    ),
                    host_socket: params.host_socket,
                })
                .await;
            let response = match result {
                Ok(context) => PrepareWaylandResponse::Ready {
                    guest_socket: context.guest_socket().to_path_buf(),
                    uid: context.identity().uid(),
                    gid: context.identity().gid(),
                },
                Err(error) => PrepareWaylandResponse::Failed {
                    message: error.to_string(),
                    hint: error.hint().map(str::to_owned),
                },
            };
            HandleOutcome::Sync(serde_json::to_value(response).map_err(|error| error.to_string()))
        }
        RpcMethod::ProbeX11Projection => {
            let params: ProbeX11ProjectionParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid probe_x11_projection request: {error}"
                    )))
                }
            };
            let resolver = crate::adapters::session::X11SessionResolver::new(
                machine,
                crate::adapters::config::NspawnConfigStore::direct(),
            );
            let result = resolver
                .probe(crate::application::sessions::X11ProjectionProbeRequest {
                    probe_id: crate::domain::session::SessionId::new(params.probe_id.get())
                        .expect("wire session id is non-zero"),
                    target: crate::application::sessions::ShellTarget::new(
                        params.machine,
                        params.user,
                    ),
                    host_socket: params.host_socket,
                })
                .await;
            let response = match result {
                Ok(context) => {
                    let identity = context.identity();
                    let guest = identity.guest();
                    let instance = identity.instance();
                    let pid_namespace = instance.pid_namespace();
                    let user_namespace = instance.user_namespace();
                    ProbeX11ProjectionResponse::Ready {
                        guest_mount: context.guest_mount().to_path_buf(),
                        guest_client_path: context.guest_client_path().to_path_buf(),
                        guest_uid: guest.uid(),
                        guest_gid: guest.gid(),
                        host_uid: identity.host_uid(),
                        host_gid: identity.host_gid(),
                        leader_pid: instance.leader_pid(),
                        pid_namespace_device: pid_namespace.device(),
                        pid_namespace_inode: pid_namespace.inode(),
                        user_namespace_device: user_namespace.device(),
                        user_namespace_inode: user_namespace.inode(),
                    }
                }
                Err(error) => ProbeX11ProjectionResponse::Failed {
                    message: error.to_string(),
                    hint: error.hint().map(str::to_owned),
                },
            };
            HandleOutcome::Sync(serde_json::to_value(response).map_err(|error| error.to_string()))
        }
        _ => unreachable!("non-session method routed to session dispatcher"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_rpc_family_contains_only_the_closed_session_methods() {
        for method in RpcMethod::ALL {
            if method.family() == RpcFamily::Session {
                assert!(matches!(
                    method,
                    RpcMethod::CloseSession
                        | RpcMethod::PrepareWayland
                        | RpcMethod::ProbeX11Projection
                ));
            }
        }
    }
}
