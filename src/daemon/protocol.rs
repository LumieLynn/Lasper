//! Private, closed wire types shared by the daemon client and server.

use crate::adapters::provisioning::engine::bootstrap_operation::BootstrapRequest;
use crate::adapters::provisioning::engine::image_operation::ImportTarRequest;
use crate::adapters::provisioning::engine::oci_operation::OciPullRequest;
use crate::adapters::rootfs::store::RootfsOperation;
use crate::domain::secret::SecretBytes;
use crate::nspawn::models::MachineName;
use serde::{Deserialize, Serialize};

use super::session_protocol::{SpawnJournalctlParams, SpawnTerminalParams};

pub(crate) const RPC_PROTOCOL_VERSION: u32 = 11;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpcBootstrap {
    pub protocol_version: u32,
    pub auth_token: String,
    pub dbus_enabled: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CliInspectMachineRequest {
    pub machine: MachineName,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FdRequest {
    pub auth_token: String,
    #[serde(flatten)]
    pub operation: FdOperation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub(crate) enum FdOperation {
    #[serde(rename = "spawn_journalctl")]
    Journalctl(SpawnJournalctlParams),
    #[serde(rename = "spawn_terminal")]
    Terminal(SpawnTerminalParams),
    #[serde(rename = "spawn_bootstrap")]
    Bootstrap(Box<SpawnBootstrapParams>),
    #[serde(rename = "spawn_oci_pull")]
    OciPull(Box<SpawnOciPullParams>),
    #[serde(rename = "import_raw_image")]
    ImportRawImage(ImportRawImageParams),
    #[serde(rename = "import_tar_image")]
    ImportTarImage(ImportTarRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnBootstrapParams {
    pub cmd_id: u64,
    pub request: BootstrapRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnOciPullParams {
    pub cmd_id: u64,
    pub request: OciPullRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportRawImageParams {
    pub machine: MachineName,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportImageResponse {
    #[serde(default)]
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

pub(crate) enum OutboundRpcRequest {
    General(RpcRequest),
    Rootfs { id: u64, operation: RootfsOperation },
}

impl OutboundRpcRequest {
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::General(request) => request.id,
            Self::Rootfs { id, .. } => *id,
        }
    }

    pub(crate) fn method(&self) -> &str {
        match self {
            Self::General(request) => &request.method,
            Self::Rootfs { .. } => "rootfs",
        }
    }

    pub(crate) fn into_wire_bytes(self) -> serde_json::Result<SecretBytes> {
        #[derive(Serialize)]
        struct RootfsEnvelope<'a> {
            jsonrpc: &'static str,
            id: u64,
            method: &'static str,
            params: &'a RootfsOperation,
        }

        let bytes = match self {
            Self::General(request) => serde_json::to_vec(&request)?,
            Self::Rootfs { id, operation } => serde_json::to_vec(&RootfsEnvelope {
                jsonrpc: "2.0",
                id,
                method: "rootfs",
                params: &operation,
            })?,
        };
        Ok(SecretBytes::new(bytes))
    }
}

pub(crate) type RpcCall = (
    OutboundRpcRequest,
    tokio::sync::oneshot::Sender<RpcResponse>,
);
