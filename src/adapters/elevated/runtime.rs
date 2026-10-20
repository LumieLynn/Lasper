//! RPC proxies for machine/image lifecycle, host operations, and inspection.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::system_operation::SystemOperation;
use crate::application::image_lifecycle::{ImageControlOutcome, ImageRemoveRequest};
use crate::application::machine_lifecycle::{
    MachineControlOutcome, MachinePreparationError, MachineRuntimeControlRequest,
    MachineStartPreparationRequest, NspawnLaunchRequest, NspawnUnitControlRequest,
};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::ipc::protocol::{error_code, InspectMachineRequest, RpcError};

impl ElevatedDaemon {
    pub(crate) async fn system_operation(
        &self,
        operation: SystemOperation,
        transport: crate::application::machine_lifecycle::MachineControlTransport,
    ) -> std::io::Result<()> {
        let params = serde_json::to_value(crate::ipc::protocol::system::SystemOperationRequest {
            operation: operation.into(),
            transport,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc.call("system_operation", params).await?;
        Ok(())
    }

    pub(crate) async fn image_remove(
        &self,
        request: ImageRemoveRequest,
    ) -> std::io::Result<ImageControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("image_remove", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn nspawn_launch(
        &self,
        request: NspawnLaunchRequest,
    ) -> std::io::Result<MachineControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("nspawn_launch", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn machine_runtime_control(
        &self,
        request: MachineRuntimeControlRequest,
    ) -> std::io::Result<MachineControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("machine_runtime_control", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn nspawn_unit_control(
        &self,
        request: NspawnUnitControlRequest,
    ) -> std::io::Result<MachineControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("nspawn_unit_control", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn systemd_tools_inspect_machine(
        &self,
        name: &str,
        include_nspawn_unit: bool,
    ) -> std::io::Result<MachineProperties> {
        let request = InspectMachineRequest {
            machine: MachineName::new(name)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
            include_nspawn_unit,
        };
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self
            .rpc
            .call("systemd_tools_inspect_machine", params)
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn systemd_tools_inspect_unit(
        &self,
        machine: &MachineName,
    ) -> std::io::Result<MachineProperties> {
        let result = self
            .rpc
            .call("systemd_tools_inspect_unit", serde_json::json!(machine))
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn prepare_machine_start(
        &self,
        request: MachineStartPreparationRequest,
    ) -> Result<(), MachinePreparationError> {
        let params = serde_json::to_value(request)
            .map_err(|error| MachinePreparationError::invalid_configuration(error.to_string()))?;
        let result = self
            .rpc
            .call("prepare_machine_start", params)
            .await
            .map_err(map_rpc_error)?;
        decode_reply(result)
    }
}

fn decode_reply(value: serde_json::Value) -> Result<(), MachinePreparationError> {
    serde_json::from_value(value)
        .map_err(|error| MachinePreparationError::outcome_unknown(error.to_string()))?
}

fn map_rpc_error(error: std::io::Error) -> MachinePreparationError {
    if let Some(remote) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RpcError>())
    {
        return match remote.code {
            error_code::INVALID_REQUEST
            | error_code::INVALID_PARAMS
            | error_code::METHOD_NOT_FOUND => {
                MachinePreparationError::invalid_configuration(remote.to_string())
            }
            error_code::REQUEST_LIMIT | error_code::RESOURCE_BUSY => {
                MachinePreparationError::busy(remote.to_string())
            }
            _ => MachinePreparationError::outcome_unknown(remote.to_string()),
        };
    }
    MachinePreparationError::outcome_unknown(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_host_errors_keep_their_semantics_across_the_reply() {
        for error in [
            MachinePreparationError::permission_denied("NVIDIA state"),
            MachinePreparationError::invalid_configuration("managed block"),
            MachinePreparationError::failed("disk full"),
            MachinePreparationError::outcome_unknown("manager reload timed out"),
        ] {
            let result: Result<(), MachinePreparationError> = Err(error.clone());
            assert_eq!(
                decode_reply(serde_json::to_value(result).unwrap()),
                Err(error)
            );
        }
    }

    #[test]
    fn lost_or_malformed_reply_cannot_be_reported_as_a_known_failure() {
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::InvalidData,
        ] {
            assert!(map_rpc_error(std::io::Error::new(kind, "reply lost")).is_outcome_unknown());
        }
        assert!(decode_reply(serde_json::Value::Null)
            .unwrap_err()
            .is_outcome_unknown());
    }

    #[test]
    fn admission_rejections_prove_that_preparation_did_not_start() {
        for code in [error_code::REQUEST_LIMIT, error_code::RESOURCE_BUSY] {
            let error = map_rpc_error(std::io::Error::other(RpcError::new(code, "busy")));
            assert!(error.is_busy());
            assert!(!error.is_outcome_unknown());
        }
        let error = map_rpc_error(std::io::Error::other(RpcError::new(
            error_code::INVALID_PARAMS,
            "invalid target",
        )));
        assert!(error.is_invalid_configuration());
        assert!(!error.is_outcome_unknown());
    }
}
