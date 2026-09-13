//! Narrow wire DTOs for privileged rootfs operations.
//!
//! These types intentionally contain no host paths, command runners, or
//! adapter implementation types. The rootfs adapter owns validation,
//! path-resolution policy, and conversion to its execution model.

use crate::domain::secret::{serde_secret, SecretString};
use crate::domain::wayland::ContainerUserIdentity;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum RootfsOperation {
    ProbeOsRelease(TargetRequest),
    ProbeNspawnCommandSupport(TargetRequest),
    MountManagedRaw(TargetRequest),
    UnmountManagedRaw(TargetRequest),
    ConfigureHostname(ConfigureHostnameRequest),
    ConfigureNetwork(TargetRequest),
    SetRootPassword(SetRootPasswordRequest),
    CreateUser(CreateUserRequest),
    ResolveUserIdentity(ResolveUserIdentityRequest),
    ConfigureNvidia(ConfigureNvidiaRequest),
    CleanupNvidia(CleanupNvidiaRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RootfsTarget {
    Machine { machine: String },
    ImageMount { machine: String },
    RawMount { machine: String, mount_id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetRequest {
    pub(crate) target: RootfsTarget,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigureHostnameRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) hostname: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetRootPasswordRequest {
    pub(crate) target: RootfsTarget,
    #[serde(with = "serde_secret")]
    pub(crate) password: SecretString,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateUserRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) username: String,
    #[serde(default)]
    pub(crate) uid: Option<u32>,
    #[serde(with = "serde_secret::optional")]
    pub(crate) password: Option<SecretString>,
    pub(crate) sudoer: bool,
    pub(crate) shell: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveUserIdentityRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) username: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigureNvidiaRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) ld_cache_folders: Vec<String>,
    pub(crate) environment: Vec<(String, String)>,
    pub(crate) write_environment: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupNvidiaRequest {
    pub(crate) target: RootfsTarget,
    pub(crate) paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootfsResult {
    pub(crate) present: Option<bool>,
    #[serde(default)]
    pub(crate) warnings: Vec<String>,
    #[serde(default)]
    pub(crate) identity: Option<ContainerUserIdentity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootfs_operation_rejects_unknown_top_level_fields() {
        let value = serde_json::json!({
            "operation": "probe_os_release",
            "params": {"target": {"kind": "machine", "machine": "test"}},
            "authority": "unexpected"
        });
        assert!(serde_json::from_value::<RootfsOperation>(value).is_err());
    }
}
