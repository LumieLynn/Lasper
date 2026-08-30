//! Session-family JSON-RPC handlers.
//!
//! Process spawning and session tracking live in `sessions::server`; this
//! module owns only the typed control-plane transition exposed to the RPC
//! dispatcher.

pub(crate) mod server;

use super::dispatch::handler::HandleOutcome;
use super::server::DaemonServerState;
use crate::ipc::protocol::session::CloseSessionParams;
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct SessionContext {
    pub(super) params: Value,
    pub(super) server_state: Arc<DaemonServerState>,
}

pub(crate) async fn handle(method: RpcMethod, context: SessionContext) -> HandleOutcome {
    let SessionContext {
        params,
        server_state,
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
        _ => unreachable!("non-session method routed to session dispatcher"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_close_session_is_in_the_session_rpc_family() {
        for method in RpcMethod::ALL {
            if method.family() == RpcFamily::Session {
                assert!(matches!(method, RpcMethod::CloseSession));
            }
        }
    }
}
