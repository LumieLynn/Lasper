//! Session control, FD delivery, and ownership of session child processes.

mod handoff;
pub(crate) mod journal;
mod lifecycle;
mod process;
mod registry;
pub(crate) mod terminal;

pub(crate) use registry::SessionRegistry;

use crate::daemon::dispatch::{decode_params, encode_result, HandlerResult};
use crate::ipc::protocol::session::{
    CloseSessionParams, PrepareWaylandParams, PrepareWaylandResponse, ProbeX11ProjectionParams,
    ProbeX11ProjectionResponse, WireMachineNamespaces,
};
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct SessionContext {
    pub(super) params: Value,
    pub(super) sessions: Arc<SessionRegistry>,
    pub(super) machine: crate::adapters::session::MachineSessionTransport,
    pub(super) invoking_uid: u32,
}

pub(crate) async fn handle(method: RpcMethod, context: SessionContext) -> HandlerResult {
    let SessionContext {
        params,
        sessions,
        machine,
        invoking_uid,
    } = context;
    debug_assert_eq!(method.family(), RpcFamily::Session);

    match method {
        RpcMethod::CloseSession => {
            let params: CloseSessionParams = decode_params(method, params)?;
            sessions
                .close_and_escalate(params.session_id)
                .map(|()| Value::Null)
                .map_err(|error| error.to_string())
        }
        RpcMethod::PrepareWayland => {
            let params: PrepareWaylandParams = decode_params(method, params)?;
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
            encode_result(response)
        }
        RpcMethod::ProbeX11Projection => {
            let params: ProbeX11ProjectionParams = decode_params(method, params)?;
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
                    ProbeX11ProjectionResponse::Ready {
                        guest_mount: context.guest_mount().to_path_buf(),
                        guest_client_path: context.guest_client_path().to_path_buf(),
                        mount_writable: context.filesystem_access().mount_writable(),
                        client_writable: context.filesystem_access().client_writable(),
                        guest_uid: guest.uid(),
                        guest_gid: guest.gid(),
                        host_uid: identity.host_uid(),
                        host_gid: identity.host_gid(),
                        leader_pid: instance.leader_pid(),
                        leader_start_time: instance
                            .leader_start_time()
                            .expect("X11 projection observes the leader start time"),
                        namespaces: instance.namespaces().map(|(pid, user)| {
                            WireMachineNamespaces {
                                pid: (pid.device(), pid.inode()),
                                user: (user.device(), user.inode()),
                            }
                        }),
                    }
                }
                Err(error) => ProbeX11ProjectionResponse::Failed {
                    message: error.to_string(),
                    hint: error.hint().map(str::to_owned),
                },
            };
            encode_result(response)
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
