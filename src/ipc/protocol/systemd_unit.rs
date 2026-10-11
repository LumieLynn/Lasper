//! Wire contract for Lasper-managed systemd unit drop-ins.
//!
//! The unit adapter owns path discovery, marker checks, and INI rendering.
//! Only validated machine identities and bounded content cross the privileged
//! RPC boundary.

use crate::application::image_lifecycle::ArtifactOwnership;
use crate::application::provisioning::ResourceApplyStatus;
use crate::domain::machine::MachineName;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum SystemdUnitOperation {
    Read(ReadServiceOverrides),
    ProbeOwnedOverrides(ReadServiceOverrides),
    WriteOverride(WriteServiceOverride),
    CloneOverride(CloneServiceOverride),
    WriteNvidiaDeviceAllow(WriteNvidiaDeviceAllow),
    RemoveOverride(RemoveServiceOverride),
    RemoveOwnedOverrides(RemoveServiceOverrides),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadServiceOverrides {
    pub(crate) machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteServiceOverride {
    pub(crate) machine: MachineName,
    pub(crate) spec: ServiceOverrideSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloneServiceOverride {
    pub(crate) source: MachineName,
    pub(crate) destination: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteNvidiaDeviceAllow {
    pub(crate) machine: MachineName,
    pub(crate) device_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveServiceOverrides {
    pub(crate) machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveServiceOverride {
    pub(crate) machine: MachineName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceOverrideSpec {
    pub(crate) device_binds: Vec<String>,
    #[serde(default)]
    pub(crate) gpu_passthrough_all: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdUnitResult {
    #[serde(default)]
    pub(crate) drop_ins: Vec<SystemdDropIn>,
    #[serde(default)]
    pub(crate) apply: Option<ResourceApplyStatus>,
    #[serde(default)]
    pub(crate) ownership: Option<Vec<ArtifactOwnership>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemdDropIn {
    pub(crate) path: String,
    pub(crate) content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_operations_keep_the_existing_nested_shape() {
        let value =
            serde_json::to_value(SystemdUnitOperation::WriteOverride(WriteServiceOverride {
                machine: MachineName::new("test-machine").unwrap(),
                spec: ServiceOverrideSpec {
                    device_binds: vec!["/dev/nvidia0".into()],
                    gpu_passthrough_all: true,
                },
            }))
            .unwrap();

        assert_eq!(value["operation"], "write_override");
        assert_eq!(value["params"]["machine"], "test-machine");
        assert_eq!(value["params"]["spec"]["device_binds"][0], "/dev/nvidia0");
        assert!(value["params"]["spec"]["gpu_passthrough_all"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn wire_operations_reject_unknown_fields_and_invalid_names() {
        let unknown = serde_json::json!({
            "operation": "read",
            "params": {"machine": "test", "extra": true}
        });
        assert!(serde_json::from_value::<SystemdUnitOperation>(unknown).is_err());

        let invalid = serde_json::json!({
            "operation": "read",
            "params": {"machine": "../escape"}
        });
        assert!(serde_json::from_value::<SystemdUnitOperation>(invalid).is_err());
    }
}
