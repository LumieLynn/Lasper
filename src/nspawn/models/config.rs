use crate::domain::provisioning::{
    BindMount, CreateUser, NetworkMode, PortForward, PrivateUsersMode,
};
use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskImageFilesystem {
    #[default]
    Ext4,
    Xfs,
    Btrfs,
}

impl DiskImageFilesystem {
    pub const ALL: [Self; 3] = [Self::Ext4, Self::Xfs, Self::Btrfs];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ext4 => "Ext4",
            Self::Xfs => "XFS",
            Self::Btrfs => "Btrfs",
        }
    }

    pub fn mkfs_tool(self) -> &'static str {
        match self {
            Self::Ext4 => "mkfs.ext4",
            Self::Xfs => "mkfs.xfs",
            Self::Btrfs => "mkfs.btrfs",
        }
    }

    pub fn to_index(self) -> usize {
        match self {
            Self::Ext4 => 0,
            Self::Xfs => 1,
            Self::Btrfs => 2,
        }
    }
}

impl std::fmt::Display for DiskImageFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) const MAX_DISK_IMAGE_SIZE_BYTES: u64 = 64 * 1024 * 1024 * 1024 * 1024;

pub fn parse_disk_image_size(value: &str) -> Result<u64> {
    if value.is_empty() || value.trim() != value || value.len() > 32 {
        return Err(NspawnError::Validation(
            "Invalid disk image size; use an integer such as 10G or 500M".into(),
        ));
    }
    let digit_count = value.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return Err(NspawnError::Validation(
            "Invalid disk image size; use an integer such as 10G or 500M".into(),
        ));
    }
    let amount = value[..digit_count].parse::<u64>().map_err(|_| {
        NspawnError::Validation("Disk image size is outside the supported range".into())
    })?;
    let unit = value[digit_count..].to_ascii_uppercase();
    let factor = match unit.as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024_u64.pow(2),
        "G" | "GB" | "GIB" => 1024_u64.pow(3),
        "T" | "TB" | "TIB" => 1024_u64.pow(4),
        _ => {
            return Err(NspawnError::Validation(
                "Unsupported disk image size unit; use B, K, M, G, or T".into(),
            ));
        }
    };
    let bytes = amount.checked_mul(factor).ok_or_else(|| {
        NspawnError::Validation("Disk image size is outside the supported range".into())
    })?;
    if bytes == 0 || bytes > MAX_DISK_IMAGE_SIZE_BYTES {
        return Err(NspawnError::Validation(format!(
            "Disk image size must be between 1 byte and {} bytes",
            MAX_DISK_IMAGE_SIZE_BYTES
        )));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiskImageSource {
    CreateNew {
        size: String,
        fs_type: DiskImageFilesystem,
    },
    ImportExisting {
        path: String,
    },
}

pub const MAX_DISK_IMAGE_PARTITIONS: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DiskImagePartition(u32);

impl DiskImagePartition {
    pub fn new(number: u32) -> Result<Self> {
        if (1..=MAX_DISK_IMAGE_PARTITIONS).contains(&number) {
            Ok(Self(number))
        } else {
            Err(NspawnError::Validation(format!(
                "Disk image partition must be between 1 and {MAX_DISK_IMAGE_PARTITIONS}"
            )))
        }
    }

    pub fn number(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for DiskImagePartition {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let number = u32::deserialize(deserializer)?;
        Self::new(number).map_err(serde::de::Error::custom)
    }
}

/// Configuration for disk image storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskImageConfig {
    pub source: DiskImageSource,
    pub use_partition_table: bool,
    #[serde(default)]
    pub root_partition: Option<DiskImagePartition>,
}

impl Default for DiskImageConfig {
    fn default() -> Self {
        Self {
            source: DiskImageSource::CreateNew {
                size: "10G".to_string(),
                fs_type: DiskImageFilesystem::Ext4,
            },
            use_partition_table: true,
            root_partition: None,
        }
    }
}

/// Complete configuration for a new container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerConfig {
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
    /// Whether to enable hardware graphics acceleration (Auto-detected DRI/WSL/Mali).
    pub graphics_acceleration: bool,
    /// Whether to expose the complete host DRM device directory.
    #[serde(default)]
    pub gpu_passthrough_all: bool,
    pub users: Vec<CreateUser>,
    /// Whether to enable NVIDIA GPU passthrough (JIT managed).
    pub nvidia_gpu: bool,
    /// Disk image specific configuration (only used if storage type is DiskImage).
    pub disk_config: Option<DiskImageConfig>,
    /// Whether to start an init process (Boot=yes). True by default. false for basic OCI containers.
    #[serde(default = "default_boot")]
    pub boot: bool,
}

fn default_boot() -> bool {
    true
}

impl Default for ContainerConfig {
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
    fn test_container_config_defaults() {
        let cfg = ContainerConfig::default();
        assert!(cfg.boot);
        assert_eq!(cfg.network, None);
        assert_eq!(cfg.private_users, None);
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

        let config: ContainerConfig = serde_json::from_value(value).unwrap();

        assert_eq!(config.guest_hostname, "guest.example");
    }

    #[test]
    fn disk_image_size_parser_accepts_bounded_integer_units() {
        assert_eq!(parse_disk_image_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(
            parse_disk_image_size("2GiB").unwrap(),
            2 * 1024 * 1024 * 1024
        );
        assert!(parse_disk_image_size("0G").is_err());
        assert!(parse_disk_image_size("1.5G").is_err());
        assert!(parse_disk_image_size("10XB").is_err());
        assert!(parse_disk_image_size(" 10G").is_err());
    }

    #[test]
    fn disk_image_partition_is_bounded_and_validated_on_deserialization() {
        assert_eq!(DiskImagePartition::new(1).unwrap().number(), 1);
        assert_eq!(
            DiskImagePartition::new(MAX_DISK_IMAGE_PARTITIONS)
                .unwrap()
                .number(),
            MAX_DISK_IMAGE_PARTITIONS
        );
        assert!(DiskImagePartition::new(0).is_err());
        assert!(DiskImagePartition::new(MAX_DISK_IMAGE_PARTITIONS + 1).is_err());
        assert!(serde_json::from_str::<DiskImagePartition>("0").is_err());
    }

    #[test]
    fn legacy_disk_image_config_defaults_to_automatic_root_selection() {
        let json = r#"{
            "source": {"CreateNew": {"size": "2G", "fs_type": "ext4"}},
            "use_partition_table": true
        }"#;
        let config: DiskImageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.root_partition, None);
    }

    #[test]
    fn test_container_config_serde_roundtrip() {
        let cfg = ContainerConfig {
            name: "test".into(),
            nvidia_gpu: true,
            private_users: Some(PrivateUsersMode::Managed),
            ..Default::default()
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: ContainerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, cfg2);
    }
}
