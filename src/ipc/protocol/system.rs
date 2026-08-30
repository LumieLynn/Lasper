//! Wire DTO for the small set of host system operations.
//!
//! This schema deliberately contains only validated identity values. Command
//! names, argument construction, and host policy remain in the system
//! operation adapter.

use crate::domain::machine::{AllowedSignal, MachineName};
use crate::domain::runtime::ImageName;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SystemOperation {
    Start {
        machine: MachineName,
    },
    Terminate {
        machine: MachineName,
    },
    Poweroff {
        machine: MachineName,
    },
    Reboot {
        machine: MachineName,
    },
    Enable {
        machine: MachineName,
    },
    Disable {
        machine: MachineName,
    },
    Kill {
        machine: MachineName,
        signal: AllowedSignal,
    },
    RemoveImage {
        image: ImageName,
    },
    CloneImage {
        source: ImageName,
        destination: ImageName,
    },
    ReloadDaemon,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_operations_have_a_closed_and_stable_shape() {
        let operation = SystemOperation::Kill {
            machine: MachineName::new("test-machine").unwrap(),
            signal: AllowedSignal::Kill,
        };
        let value = serde_json::to_value(&operation).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "operation": "kill",
                "machine": "test-machine",
                "signal": "SIGKILL"
            })
        );

        let parsed: SystemOperation = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, operation);
    }

    #[test]
    fn wire_operations_reject_unknown_fields() {
        let value = serde_json::json!({
            "operation": "start",
            "machine": "test-machine",
            "program": "sh"
        });
        assert!(serde_json::from_value::<SystemOperation>(value).is_err());
    }
}
