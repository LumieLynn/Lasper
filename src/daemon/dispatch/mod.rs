//! Route validated RPC methods to their owning family.
//!
//! The connection owns admission, request tasks, and responses. Family handlers
//! await their operation and return a value; they never own the RPC connection.

pub(crate) mod backend;
pub(super) mod claims;
mod command;
mod connection;
pub(crate) mod query;

pub(super) use connection::run_rpc_request_pump;

use crate::adapters::session::MachineSessionTransport;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::daemon::dispatch::backend::{DaemonRuntimeQueries, DaemonSystemExecutor};
use crate::daemon::server::DaemonServerState;
use crate::ipc::protocol::{RpcFamily, RpcMethod};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

pub(crate) type HandlerResult = Result<Value, String>;

#[derive(Clone)]
pub(super) struct Dispatcher<B> {
    pub(super) dbus: Option<B>,
    pub(super) invoking_uid: u32,
    pub(super) server_state: Arc<DaemonServerState>,
    pub(super) trusted_state_root: TrustedStateRoot,
    pub(super) machine_sessions: MachineSessionTransport,
}

impl<B: DaemonRuntimeQueries + DaemonSystemExecutor> Dispatcher<B> {
    pub(super) async fn handle(&self, method: RpcMethod, params: Value) -> HandlerResult {
        match method.family() {
            RpcFamily::Query => query::handle(method, params, &self.dbus).await,
            RpcFamily::Command => {
                command::handle(
                    method,
                    command::CommandContext {
                        params,
                        dbus: &self.dbus,
                        invoking_uid: self.invoking_uid,
                        server_state: Arc::clone(&self.server_state),
                        trusted_state_root: self.trusted_state_root.clone(),
                    },
                )
                .await
            }
            RpcFamily::Job => {
                crate::daemon::jobs::handle(
                    method,
                    crate::daemon::jobs::JobContext {
                        params,
                        dbus: &self.dbus,
                        server_state: Arc::clone(&self.server_state),
                        trusted_state_root: self.trusted_state_root.clone(),
                    },
                )
                .await
            }
            RpcFamily::Session => {
                crate::daemon::sessions::handle(
                    method,
                    crate::daemon::sessions::SessionContext {
                        params,
                        sessions: Arc::clone(&self.server_state.sessions),
                        machine: self.machine_sessions.clone(),
                        invoking_uid: self.invoking_uid,
                    },
                )
                .await
            }
        }
    }
}

pub(super) fn decode_params<T: DeserializeOwned>(
    method: RpcMethod,
    params: Value,
) -> Result<T, String> {
    serde_json::from_value(params)
        .map_err(|error| format!("invalid {} request: {error}", method.wire_name()))
}

pub(super) fn encode_result<T: Serialize>(result: T) -> HandlerResult {
    serde_json::to_value(result).map_err(|error| error.to_string())
}
