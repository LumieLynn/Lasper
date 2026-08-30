//! Private, closed wire types shared by the daemon client and server.

use crate::domain::machine::MachineName;
use crate::domain::secret::SecretBytes;
use serde::{Deserialize, Serialize};

pub(crate) mod deployment;
pub(crate) mod rootfs;
pub(crate) mod session;
pub(crate) mod system;

use self::deployment::SubmitDeploymentParams;
use self::session::{SpawnJournalctlParams, SpawnTerminalParams};

pub(crate) const RPC_PROTOCOL_VERSION: u32 = 15;

/// Stable JSON-RPC error codes used by the daemon envelope and scheduler.
/// Operation-specific semantic failures are migrated separately.
pub(crate) mod error_code {
    pub(crate) const PARSE_ERROR: i32 = -32700;
    pub(crate) const INVALID_REQUEST: i32 = -32600;
    pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
    pub(crate) const INVALID_PARAMS: i32 = -32602;
    pub(crate) const INTERNAL_ERROR: i32 = -32603;
    pub(crate) const REQUEST_LIMIT: i32 = -32001;
    pub(crate) const RESOURCE_BUSY: i32 = -32002;
}

/// Protocol families are wire-contract properties, not inferences from
/// implementation duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RpcFamily {
    Query,
    Command,
    Job,
    Session,
}

impl RpcFamily {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Command => "command",
            Self::Job => "job",
            Self::Session => "session",
        }
    }
}

/// Closed inventory of methods accepted on the authenticated JSON-RPC
/// channel. Adding a method is an explicit protocol change rather than an
/// unreviewed string-match branch in dispatch.
macro_rules! rpc_methods {
    ($( $variant:ident => ($wire_name:literal, $family:ident) ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum RpcMethod {
            $( $variant, )+
        }

        impl RpcMethod {
            pub(crate) const ALL: &'static [Self] = &[
                $( Self::$variant, )+
            ];

            pub(crate) fn parse(name: &str) -> Option<Self> {
                match name {
                    $( $wire_name => Some(Self::$variant), )+
                    _ => None,
                }
            }

            pub(crate) const fn wire_name(self) -> &'static str {
                match self {
                    $( Self::$variant => $wire_name, )+
                }
            }

            pub(crate) const fn family(self) -> RpcFamily {
                match self {
                    $( Self::$variant => RpcFamily::$family, )+
                }
            }
        }
    };
}

rpc_methods! {
    Ping => ("ping", Query),
    Exit => ("exit", Command),
    CloseSession => ("close_session", Session),
    NspawnConfig => ("nspawn_config", Command),
    SystemdUnit => ("systemd_unit", Command),
    NvidiaState => ("nvidia_state", Command),
    DeploymentState => ("deployment_state", Command),
    DeploymentStatus => ("deployment_status", Job),
    ResolveDeploymentSubmission => ("resolve_deployment_submission", Job),
    AcknowledgeDeploymentSubmission => ("acknowledge_deployment_submission", Job),
    CancelDeployment => ("cancel_deployment", Job),
    AcknowledgeDeployment => ("acknowledge_deployment", Job),
    ProbeDeploymentRecovery => ("probe_deployment_recovery", Job),
    ReconcileDeployment => ("reconcile_deployment", Job),
    ReleaseUnresolvedDeployment => ("release_unresolved_deployment", Job),
    Rootfs => ("rootfs", Command),
    AssessTarRuntime => ("assess_tar_runtime", Query),
    SystemOperation => ("system_operation", Command),
    CliInspectMachine => ("cli_inspect_machine", Query),
    DbusListMachines => ("dbus_list_machines", Query),
    DbusListImages => ("dbus_list_images", Query),
    NspawnLaunch => ("nspawn_launch", Command),
    MachineRuntimeControl => ("machine_runtime_control", Command),
    NspawnUnitControl => ("nspawn_unit_control", Command),
    ImageRemove => ("image_remove", Command),
    DbusGetProperties => ("dbus_get_properties", Query),
    DbusIsAvailable => ("dbus_is_available", Query),
}

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
    #[serde(rename = "submit_deployment")]
    SubmitDeployment(Box<SubmitDeploymentParams>),
}

impl FdOperation {
    pub(crate) const fn wire_name(&self) -> &'static str {
        match self {
            Self::Journalctl(_) => "spawn_journalctl",
            Self::Terminal(_) => "spawn_terminal",
            Self::SubmitDeployment(_) => "submit_deployment",
        }
    }

    pub(crate) const fn family(&self) -> RpcFamily {
        match self {
            Self::Journalctl(_) | Self::Terminal(_) => RpcFamily::Session,
            Self::SubmitDeployment(_) => RpcFamily::Job,
        }
    }
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

impl RpcRequest {
    pub(crate) fn method_kind(&self) -> Result<RpcMethod, RpcError> {
        if self.jsonrpc != "2.0" {
            return Err(RpcError::new(
                error_code::INVALID_REQUEST,
                "JSON-RPC version must be 2.0",
            ));
        }

        let method = RpcMethod::parse(&self.method).ok_or_else(|| {
            RpcError::new(
                error_code::METHOD_NOT_FOUND,
                format!("unknown RPC method: {}", self.method),
            )
        })?;
        debug_assert!(RpcMethod::ALL.contains(&method));
        Ok(method)
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub(crate) fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(crate) enum OutboundRpcRequest {
    General(RpcRequest),
}

impl OutboundRpcRequest {
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::General(request) => request.id,
        }
    }

    pub(crate) fn into_wire_bytes(self) -> serde_json::Result<SecretBytes> {
        let bytes = match self {
            Self::General(request) => serde_json::to_vec(&request)?,
        };
        Ok(SecretBytes::new(bytes))
    }
}

pub(crate) type RpcCall = (
    OutboundRpcRequest,
    tokio::sync::oneshot::Sender<RpcResponse>,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_method_inventory_round_trips_every_wire_name() {
        let mut names = std::collections::HashSet::new();
        for method in RpcMethod::ALL {
            assert!(names.insert(method.wire_name()));
            assert_eq!(RpcMethod::parse(method.wire_name()), Some(*method));
        }
        assert_eq!(names.len(), RpcMethod::ALL.len());
    }

    #[test]
    fn rpc_method_classification_rejects_unknown_and_wrong_version() {
        let unknown = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "not_a_method".into(),
            params: serde_json::Value::Null,
        };
        assert_eq!(
            unknown.method_kind().unwrap_err(),
            RpcError::new(
                error_code::METHOD_NOT_FOUND,
                "unknown RPC method: not_a_method"
            )
        );

        let wrong_version = RpcRequest {
            jsonrpc: "1.0".into(),
            ..unknown
        };
        assert_eq!(
            wrong_version.method_kind().unwrap_err(),
            RpcError::new(error_code::INVALID_REQUEST, "JSON-RPC version must be 2.0")
        );
    }

    #[test]
    fn rpc_families_match_operation_semantics() {
        assert_eq!(RpcMethod::DbusListMachines.family(), RpcFamily::Query);
        assert_eq!(RpcMethod::SystemdUnit.family(), RpcFamily::Command);
        assert_eq!(RpcMethod::DeploymentStatus.family(), RpcFamily::Job);
        assert_eq!(RpcMethod::CloseSession.family(), RpcFamily::Session);

        let terminal = FdOperation::Terminal(SpawnTerminalParams {
            session_id: crate::ipc::protocol::session::WireSessionId::new(1).unwrap(),
            name: MachineName::new("machine").unwrap(),
            size: crate::domain::session::SessionSize::new(80, 24)
                .unwrap()
                .into(),
        });
        assert_eq!(terminal.wire_name(), "spawn_terminal");
        assert_eq!(terminal.family(), RpcFamily::Session);
    }
}
