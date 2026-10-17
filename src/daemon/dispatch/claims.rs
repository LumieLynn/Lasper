//! Resource access required while a control-channel request is executing.

use crate::adapters::config::store::NspawnConfigOperation;
use crate::adapters::config::systemd_unit::SystemdUnitOperation;
use crate::adapters::platform::nvidia::state::NvidiaStateOperation;
use crate::adapters::system_operation::SystemOperation;
use crate::application::image_lifecycle::ImageRemoveRequest;
use crate::application::machine_lifecycle::{
    MachineRuntimeControlRequest, MachineStartPreparationRequest, NspawnLaunchRequest,
    NspawnUnitControlRequest,
};
use crate::ipc::protocol::RpcMethod;

pub(in crate::daemon) fn resource_claims(
    method: RpcMethod,
    params: &serde_json::Value,
) -> Result<Vec<crate::application::ResourceClaim>, String> {
    use crate::application::{ResourceClaim, ResourceKey};

    let claims = match method {
        RpcMethod::NspawnConfig => {
            let operation: NspawnConfigOperation = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid nspawn_config request: {error}"))?;
            operation.resource_claims()
        }
        RpcMethod::SystemdUnit => {
            let operation: crate::ipc::protocol::systemd_unit::SystemdUnitOperation =
                serde_json::from_value(params.clone())
                    .map_err(|error| format!("invalid systemd_unit request: {error}"))?;
            SystemdUnitOperation::from(operation).resource_claims()
        }
        RpcMethod::NvidiaState => {
            let operation: NvidiaStateOperation = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid nvidia_state request: {error}"))?;
            operation.resource_claims()
        }
        RpcMethod::PrepareMachineStart => {
            let request: MachineStartPreparationRequest = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid prepare_machine_start request: {error}"))?;
            request.resource_claims()
        }
        RpcMethod::ImageRemove => {
            let request: ImageRemoveRequest = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid image_remove request: {error}"))?;
            vec![ResourceClaim::exclusive(ResourceKey::for_image(
                &request.image,
            ))]
        }
        RpcMethod::NspawnLaunch => {
            let request: NspawnLaunchRequest = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid nspawn_launch request: {error}"))?;
            if !request.validates_same_name_route() {
                return Err(
                    "invalid nspawn_launch request: image and machine names must match".into(),
                );
            }
            vec![ResourceClaim::exclusive(ResourceKey::for_image(
                &request.image,
            ))]
        }
        RpcMethod::MachineRuntimeControl => {
            let request: MachineRuntimeControlRequest = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid machine_runtime_control request: {error}"))?;
            vec![ResourceClaim::exclusive(ResourceKey::for_machine(
                &request.machine,
            ))]
        }
        RpcMethod::NspawnUnitControl => {
            let request: NspawnUnitControlRequest = serde_json::from_value(params.clone())
                .map_err(|error| format!("invalid nspawn_unit_control request: {error}"))?;
            vec![
                ResourceClaim::exclusive(ResourceKey::for_machine(&request.machine)),
                ResourceClaim::shared(ResourceKey::SystemdManager),
            ]
        }
        RpcMethod::Configuration => {
            let operation: crate::ipc::protocol::configuration::ConfigurationOperation =
                serde_json::from_value(params.clone())
                    .map_err(|error| format!("invalid configuration request: {error}"))?;
            match operation {
                crate::ipc::protocol::configuration::ConfigurationOperation::Apply(edit) => {
                    use crate::application::configuration::ConfigurationTarget;
                    let key = match edit.target {
                        ConfigurationTarget::Machine(machine) => ResourceKey::for_machine(&machine),
                        ConfigurationTarget::Image(image) => ResourceKey::for_image(&image),
                    };
                    vec![ResourceClaim::exclusive(key)]
                }
                crate::ipc::protocol::configuration::ConfigurationOperation::Inspect(_)
                | crate::ipc::protocol::configuration::ConfigurationOperation::Preview(_) => vec![],
            }
        }
        RpcMethod::SystemOperation => {
            let wire_request: crate::ipc::protocol::system::SystemOperationRequest =
                serde_json::from_value(params.clone())
                    .map_err(|error| format!("invalid {} request: {error}", method.wire_name()))?;
            let operation = SystemOperation::from(wire_request.operation);
            system_operation_claims(operation)
        }
        // Deployment jobs reserve their complete plan when accepted. State
        // manifests use revision checks; queries and sessions do not mutate the
        // machine's persistent configuration. Keep this inventory exhaustive.
        RpcMethod::DeploymentState
        | RpcMethod::DeploymentStatus
        | RpcMethod::ResolveDeploymentSubmission
        | RpcMethod::AcknowledgeDeploymentSubmission
        | RpcMethod::CancelDeployment
        | RpcMethod::AcknowledgeDeployment
        | RpcMethod::ProbeDeploymentRecovery
        | RpcMethod::ReconcileDeployment
        | RpcMethod::ReleaseUnresolvedDeployment
        | RpcMethod::Ping
        | RpcMethod::Exit
        | RpcMethod::CloseSession
        | RpcMethod::PrepareWayland
        | RpcMethod::ProbeX11Projection
        | RpcMethod::AssessTarRuntime
        | RpcMethod::SystemdToolsInspectMachine
        | RpcMethod::SystemdToolsInspectUnit
        | RpcMethod::DbusListMachines
        | RpcMethod::DbusListImages
        | RpcMethod::DbusGetProperties
        | RpcMethod::DbusGetUnitProperties
        | RpcMethod::DbusIsAvailable => Vec::new(),
    };
    Ok(claims)
}

fn system_operation_claims(operation: SystemOperation) -> Vec<crate::application::ResourceClaim> {
    use crate::application::{ResourceClaim, ResourceKey};

    match operation {
        SystemOperation::Start { machine }
        | SystemOperation::Terminate { machine }
        | SystemOperation::Poweroff { machine }
        | SystemOperation::Reboot { machine }
        | SystemOperation::Kill { machine, .. } => {
            vec![ResourceClaim::exclusive(ResourceKey::for_machine(&machine))]
        }
        SystemOperation::Enable { machine } | SystemOperation::Disable { machine } => vec![
            ResourceClaim::exclusive(ResourceKey::for_machine(&machine)),
            ResourceClaim::shared(ResourceKey::SystemdManager),
        ],
        SystemOperation::RemoveImage { image } => {
            vec![ResourceClaim::exclusive(ResourceKey::for_image(&image))]
        }
        SystemOperation::CloneImage {
            source,
            destination,
        } => vec![
            ResourceClaim::shared(ResourceKey::for_image(&source)),
            ResourceClaim::exclusive(ResourceKey::for_image(&destination)),
        ],
        SystemOperation::ReloadDaemon => {
            vec![ResourceClaim::exclusive(ResourceKey::SystemdManager)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{OperationRegistry, ResourceClaim, ResourceKey};
    use crate::domain::machine::MachineName;

    #[test]
    fn preparation_excludes_conflicting_writes_until_the_whole_operation_finishes() {
        let operations = OperationRegistry::new();
        let preparation = resource_claims(
            RpcMethod::PrepareMachineStart,
            &serde_json::json!({
                "machine": "guest", "nvidia_cdi_source": {"mode": "generate"},
                "transport": "dbus"
            }),
        )
        .unwrap();
        let reservation = operations.reserve(preparation).unwrap();
        let writes = [
            (
                RpcMethod::NspawnConfig,
                serde_json::json!({
                    "operation": "remove", "params": {"machine": "guest"}
                }),
            ),
            (
                RpcMethod::NvidiaState,
                serde_json::json!({
                    "operation": "remove_owned", "params": {"machine": "guest"}
                }),
            ),
            (
                RpcMethod::SystemdUnit,
                serde_json::json!({
                    "operation": "remove_owned_overrides", "params": {"machine": "guest"}
                }),
            ),
            (
                RpcMethod::NspawnLaunch,
                serde_json::json!({
                    "image": "guest", "machine": "guest", "transport": "systemd_tools"
                }),
            ),
            (
                RpcMethod::SystemOperation,
                serde_json::json!({
                    "operation": {"operation": "reload_daemon"}, "transport": "dbus"
                }),
            ),
        ];

        for (method, params) in &writes {
            let claims = resource_claims(*method, params).unwrap();
            assert!(
                operations.reserve(claims).is_err(),
                "{} escaped preparation",
                method.wire_name()
            );
        }
        // Metadata remains observable, and an unrelated target is independent.
        assert!(resource_claims(
            RpcMethod::NvidiaState,
            &serde_json::json!({
                "operation": "read", "params": {"machine": "guest"}
            })
        )
        .unwrap()
        .is_empty());
        let other = MachineName::new("other").unwrap();
        assert!(operations
            .reserve([ResourceClaim::exclusive(ResourceKey::for_machine(&other))])
            .is_ok());

        drop(reservation);
        for (method, params) in &writes {
            assert!(operations
                .reserve(resource_claims(*method, params).unwrap())
                .is_ok());
        }
    }

    #[test]
    fn cloning_unit_overrides_protects_the_source_and_destination() {
        let operations = OperationRegistry::new();
        let claims = resource_claims(RpcMethod::SystemdUnit, &serde_json::json!({
            "operation": "clone_override", "params": {"source": "source", "destination": "destination"}
        })).unwrap();
        let _reservation = operations.reserve(claims).unwrap();

        for name in ["source", "destination"] {
            assert!(operations
                .reserve([ResourceClaim::exclusive(ResourceKey::for_machine(
                    &MachineName::new(name).unwrap()
                ))])
                .is_err());
        }
        assert!(operations
            .reserve([ResourceClaim::shared(ResourceKey::for_machine(
                &MachineName::new("source").unwrap()
            ))])
            .is_ok());
    }
}
