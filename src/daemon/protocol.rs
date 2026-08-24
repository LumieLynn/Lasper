//! Private, closed wire types shared by the daemon client and server.

use crate::adapters::rootfs::store::RootfsOperation;
use crate::domain::secret::SecretBytes;
use crate::nspawn::models::MachineName;
use serde::{Deserialize, Serialize};

use super::deployment_protocol::SubmitDeploymentParams;
use super::session_protocol::{SpawnJournalctlParams, SpawnTerminalParams};

pub(crate) const RPC_PROTOCOL_VERSION: u32 = 13;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RpcMethod {
    Ping,
    Exit,
    CloseSession,
    NspawnConfig,
    SystemdUnit,
    NvidiaState,
    DeploymentState,
    DeploymentStatus,
    ResolveDeploymentSubmission,
    AcknowledgeDeploymentSubmission,
    CancelDeployment,
    AcknowledgeDeployment,
    ProbeDeploymentRecovery,
    ReconcileDeployment,
    ReleaseUnresolvedDeployment,
    Rootfs,
    AssessTarRuntime,
    SystemOperation,
    CliInspectMachine,
    DbusListMachines,
    DbusListImages,
    DbusSystemOperation,
    MachineControl,
    ImageRemove,
    DbusGetProperties,
    DbusIsAvailable,
}

impl RpcMethod {
    pub(crate) const ALL: &'static [Self] = &[
        Self::Ping,
        Self::Exit,
        Self::CloseSession,
        Self::NspawnConfig,
        Self::SystemdUnit,
        Self::NvidiaState,
        Self::DeploymentState,
        Self::DeploymentStatus,
        Self::ResolveDeploymentSubmission,
        Self::AcknowledgeDeploymentSubmission,
        Self::CancelDeployment,
        Self::AcknowledgeDeployment,
        Self::ProbeDeploymentRecovery,
        Self::ReconcileDeployment,
        Self::ReleaseUnresolvedDeployment,
        Self::Rootfs,
        Self::AssessTarRuntime,
        Self::SystemOperation,
        Self::CliInspectMachine,
        Self::DbusListMachines,
        Self::DbusListImages,
        Self::DbusSystemOperation,
        Self::MachineControl,
        Self::ImageRemove,
        Self::DbusGetProperties,
        Self::DbusIsAvailable,
    ];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ping" => Self::Ping,
            "exit" => Self::Exit,
            "close_session" => Self::CloseSession,
            "nspawn_config" => Self::NspawnConfig,
            "systemd_unit" => Self::SystemdUnit,
            "nvidia_state" => Self::NvidiaState,
            "deployment_state" => Self::DeploymentState,
            "deployment_status" => Self::DeploymentStatus,
            "resolve_deployment_submission" => Self::ResolveDeploymentSubmission,
            "acknowledge_deployment_submission" => Self::AcknowledgeDeploymentSubmission,
            "cancel_deployment" => Self::CancelDeployment,
            "acknowledge_deployment" => Self::AcknowledgeDeployment,
            "probe_deployment_recovery" => Self::ProbeDeploymentRecovery,
            "reconcile_deployment" => Self::ReconcileDeployment,
            "release_unresolved_deployment" => Self::ReleaseUnresolvedDeployment,
            "rootfs" => Self::Rootfs,
            "assess_tar_runtime" => Self::AssessTarRuntime,
            "system_operation" => Self::SystemOperation,
            "cli_inspect_machine" => Self::CliInspectMachine,
            "dbus_list_machines" => Self::DbusListMachines,
            "dbus_list_images" => Self::DbusListImages,
            "dbus_system_operation" => Self::DbusSystemOperation,
            "machine_control" => Self::MachineControl,
            "image_remove" => Self::ImageRemove,
            "dbus_get_properties" => Self::DbusGetProperties,
            "dbus_is_available" => Self::DbusIsAvailable,
            _ => return None,
        })
    }

    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Exit => "exit",
            Self::CloseSession => "close_session",
            Self::NspawnConfig => "nspawn_config",
            Self::SystemdUnit => "systemd_unit",
            Self::NvidiaState => "nvidia_state",
            Self::DeploymentState => "deployment_state",
            Self::DeploymentStatus => "deployment_status",
            Self::ResolveDeploymentSubmission => "resolve_deployment_submission",
            Self::AcknowledgeDeploymentSubmission => "acknowledge_deployment_submission",
            Self::CancelDeployment => "cancel_deployment",
            Self::AcknowledgeDeployment => "acknowledge_deployment",
            Self::ProbeDeploymentRecovery => "probe_deployment_recovery",
            Self::ReconcileDeployment => "reconcile_deployment",
            Self::ReleaseUnresolvedDeployment => "release_unresolved_deployment",
            Self::Rootfs => "rootfs",
            Self::AssessTarRuntime => "assess_tar_runtime",
            Self::SystemOperation => "system_operation",
            Self::CliInspectMachine => "cli_inspect_machine",
            Self::DbusListMachines => "dbus_list_machines",
            Self::DbusListImages => "dbus_list_images",
            Self::DbusSystemOperation => "dbus_system_operation",
            Self::MachineControl => "machine_control",
            Self::ImageRemove => "image_remove",
            Self::DbusGetProperties => "dbus_get_properties",
            Self::DbusIsAvailable => "dbus_is_available",
        }
    }

    pub(crate) const fn family(self) -> RpcFamily {
        match self {
            Self::Ping
            | Self::CliInspectMachine
            | Self::DbusListMachines
            | Self::DbusListImages
            | Self::DbusGetProperties
            | Self::DbusIsAvailable
            | Self::AssessTarRuntime => RpcFamily::Query,
            Self::CloseSession => RpcFamily::Session,
            Self::DeploymentStatus
            | Self::ResolveDeploymentSubmission
            | Self::AcknowledgeDeploymentSubmission
            | Self::CancelDeployment
            | Self::AcknowledgeDeployment
            | Self::ProbeDeploymentRecovery
            | Self::ReconcileDeployment
            | Self::ReleaseUnresolvedDeployment => RpcFamily::Job,
            _ => RpcFamily::Command,
        }
    }
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
            session_id: crate::daemon::session_protocol::WireSessionId::new(1).unwrap(),
            name: MachineName::new("machine").unwrap(),
            size: crate::nspawn::models::TerminalSize::new(80, 24).unwrap(),
        });
        assert_eq!(terminal.wire_name(), "spawn_terminal");
        assert_eq!(terminal.family(), RpcFamily::Session);
    }
}
