//! Configuration replies preserve operation errors separately from RPC loss.

use crate::adapters::elevated::ElevatedDaemon;
use crate::application::configuration::ConfigurationError;
use crate::ipc::protocol::configuration::{ConfigurationOperation, ConfigurationResult};
use crate::ipc::protocol::{error_code, RpcError};

impl ElevatedDaemon {
    pub(crate) async fn configuration(
        &self,
        operation: ConfigurationOperation,
    ) -> ConfigurationResult {
        let mutation = operation.is_mutation();
        let params = serde_json::to_value(operation).map_err(ConfigurationError::invalid_input)?;
        let result = self
            .rpc_call("configuration", params)
            .await
            .map_err(|error| map_rpc_error(error, mutation))?;
        decode_reply(result, mutation)
    }
}

fn decode_reply(value: serde_json::Value, mutation: bool) -> ConfigurationResult {
    serde_json::from_value(value).map_err(|error| incomplete_reply(error, mutation))?
}

fn incomplete_reply(error: impl std::fmt::Display, mutation: bool) -> ConfigurationError {
    if mutation {
        ConfigurationError::outcome_unknown(error)
    } else {
        ConfigurationError::failed(error)
    }
}

fn map_rpc_error(error: std::io::Error, mutation: bool) -> ConfigurationError {
    // Explicit admission rejections are not ambiguous writes. Other transport,
    // timeout or handler failures cannot prove that an apply did not happen.
    if let Some(remote) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RpcError>())
    {
        return match remote.code {
            error_code::INVALID_REQUEST
            | error_code::INVALID_PARAMS
            | error_code::METHOD_NOT_FOUND => ConfigurationError::invalid_input(remote),
            error_code::REQUEST_LIMIT | error_code::RESOURCE_BUSY => {
                ConfigurationError::failed(remote)
            }
            _ => incomplete_reply(remote, mutation),
        };
    }
    incomplete_reply(error, mutation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::configuration::ConfigurationApplyReport;
    use crate::ipc::protocol::configuration::ConfigurationValue;

    #[test]
    fn operation_rejections_survive_the_wire_without_string_classification() {
        for error in [
            ConfigurationError::PermissionDenied("settings directory".into()),
            ConfigurationError::invalid_input("target"),
            ConfigurationError::failed("disk full"),
        ] {
            let reply: ConfigurationResult = Err(error.clone());
            assert_eq!(
                decode_reply(serde_json::to_value(reply).unwrap(), true).unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn lost_or_malformed_apply_reply_is_unknown_but_read_failure_is_not() {
        for mutation in [false, true] {
            let error = map_rpc_error(
                std::io::Error::from(std::io::ErrorKind::BrokenPipe),
                mutation,
            );
            assert_eq!(error.is_outcome_unknown(), mutation);
            let malformed =
                decode_reply(serde_json::json!({"old_optional_snapshot": null}), mutation)
                    .unwrap_err();
            assert_eq!(malformed.is_outcome_unknown(), mutation);
        }
    }

    #[test]
    fn admission_rejection_is_not_an_unknown_save() {
        for code in [
            error_code::INVALID_PARAMS,
            error_code::RESOURCE_BUSY,
            error_code::REQUEST_LIMIT,
        ] {
            let error = map_rpc_error(std::io::Error::other(RpcError::new(code, "rejected")), true);
            assert!(!error.is_outcome_unknown());
        }
        assert!(map_rpc_error(
            std::io::Error::other(RpcError::new(error_code::INTERNAL_ERROR, "handler stopped")),
            true
        )
        .is_outcome_unknown());
    }

    #[test]
    fn revision_conflict_is_a_report_not_a_transport_failure() {
        let reply: ConfigurationResult = Ok(ConfigurationValue::Apply(
            ConfigurationApplyReport::Conflict {
                reason: "revision changed".into(),
            },
        ));
        assert!(matches!(
            decode_reply(serde_json::to_value(reply).unwrap(), true).unwrap(),
            ConfigurationValue::Apply(ConfigurationApplyReport::Conflict { .. })
        ));
    }
}
