//! Provisioning intent for creating a machine-backed systemd-nspawn guest.
//!
//! This is a workflow contract, not a lossless `.nspawn` document. Host-side
//! adapters project its runtime fields into `NspawnConfigSpec`, while source,
//! account, and storage fields remain owned by the provisioning workflow.

use crate::domain::provisioning::{
    BindMount, CreateUser, NetworkMode, PortForward, PrivateUsersMode,
};
use crate::domain::storage::DiskImageConfig;
use serde::{Deserialize, Serialize};

/// Complete provisioning configuration for a new machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineProvisioningConfig {
    pub name: String,
    /// Optional static hostname for the guest. An empty value uses `name`.
    #[serde(default, alias = "hostname")]
    pub guest_hostname: String,
    pub network: Option<NetworkMode>,
    pub port_forwards: Vec<PortForward>,
    pub bind_mounts: Vec<BindMount>,
    /// Device files to bind-mount (read-write).
    pub device_binds: Vec<String>,
    /// Paths to bind-mount (read-only).
    pub readonly_binds: Vec<String>,
    /// Whether to grant all capabilities (privileged mode).
    pub privileged: bool,
    /// Explicit `PrivateUsers=` setting. `None` keeps systemd's default policy.
    pub private_users: Option<PrivateUsersMode>,
    /// Whether to enable hardware graphics acceleration (auto-detected DRI/WSL/Mali).
    pub graphics_acceleration: bool,
    /// Whether to expose the complete host DRM device directory.
    #[serde(default)]
    pub gpu_passthrough_all: bool,
    pub users: Vec<CreateUser>,
    /// Whether to enable NVIDIA GPU passthrough (JIT managed).
    pub nvidia_gpu: bool,
    /// Disk image-specific configuration (only used if storage type is DiskImage).
    pub disk_config: Option<DiskImageConfig>,
    /// Whether to start an init process (`Boot=yes`). False is used for basic OCI guests.
    #[serde(default = "default_boot")]
    pub boot: bool,
}

fn default_boot() -> bool {
    true
}

impl Default for MachineProvisioningConfig {
    fn default() -> Self {
        Self {
            name: Default::default(),
            guest_hostname: Default::default(),
            network: Default::default(),
            port_forwards: Default::default(),
            bind_mounts: Default::default(),
            device_binds: Default::default(),
            readonly_binds: Default::default(),
            privileged: Default::default(),
            private_users: None,
            graphics_acceleration: Default::default(),
            gpu_passthrough_all: Default::default(),
            users: Default::default(),
            nvidia_gpu: Default::default(),
            disk_config: Default::default(),
            boot: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_describe_a_bootable_machine() {
        let config = MachineProvisioningConfig::default();
        assert!(config.boot);
        assert_eq!(config.network, None);
        assert_eq!(config.private_users, None);
    }

    #[test]
    fn legacy_hostname_field_deserializes_as_guest_hostname() {
        let value = serde_json::json!({
            "name": "machine",
            "hostname": "guest.example",
            "network": null,
            "port_forwards": [],
            "bind_mounts": [],
            "device_binds": [],
            "readonly_binds": [],
            "privileged": false,
            "private_users": null,
            "graphics_acceleration": false,
            "users": [],
            "nvidia_gpu": false,
            "disk_config": null
        });

        let config: MachineProvisioningConfig = serde_json::from_value(value).unwrap();

        assert_eq!(config.guest_hostname, "guest.example");
    }

    #[test]
    fn serde_roundtrip_preserves_machine_provisioning_intent() {
        let config = MachineProvisioningConfig {
            name: "test".into(),
            nvidia_gpu: true,
            private_users: Some(PrivateUsersMode::Managed),
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let restored: MachineProvisioningConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, restored);
    }
}
