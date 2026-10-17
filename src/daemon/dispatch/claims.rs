//! Resource access required while a control-channel request is executing.

use crate::adapters::system_operation::SystemOperation;
use crate::application::image_lifecycle::ImageRemoveRequest;
use crate::application::machine_lifecycle::{
    MachineRuntimeControlRequest, NspawnLaunchRequest, NspawnUnitControlRequest,
};
use crate::ipc::protocol::RpcMethod;

pub(in crate::daemon) fn resource_claims(
    method: RpcMethod,
    params: &serde_json::Value,
) -> Result<Vec<crate::application::ResourceClaim>, String> {
    use crate::application::{ResourceClaim, ResourceKey};

    let claims = match method {
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
                _ => vec![],
            }
        }
        RpcMethod::SystemOperation => {
            let wire_request: crate::ipc::protocol::system::SystemOperationRequest =
                serde_json::from_value(params.clone())
                    .map_err(|error| format!("invalid {} request: {error}", method.wire_name()))?;
            let operation = SystemOperation::from(wire_request.operation);
            system_operation_claims(operation)
        }
        _ => Vec::new(),
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
