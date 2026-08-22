use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

/// Represents the network configuration for a container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum NetworkMode {
    /// Share the host's network namespace.
    Host,
    /// Private network namespace with no connectivity (unless manually configured).
    None,
    /// Virtual Ethernet pair (veth).
    #[default]
    Veth,
    /// Connect to a specific host bridge.
    Bridge(String),
    /// MACVLAN mode (virtual independent MAC).
    MacVlan(String),
    /// IPVLAN mode (sharing host MAC).
    IpVlan(String),
    /// Physical interface passthrough.
    Interface(String),
}

impl NetworkMode {
    /// Whether this mode creates or joins a network namespace separate from the host.
    pub fn is_private(&self) -> bool {
        !matches!(self, Self::Host)
    }

    /// Whether Lasper relies on the guest's default networkd/resolved setup.
    pub const fn uses_default_guest_network_stack(&self) -> bool {
        matches!(self, Self::Veth | Self::Bridge(_))
    }
}

/// Network modes exposed for system-scoped OCI application imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OciNetworkMode {
    /// Share the host network namespace.
    #[default]
    Host,
    /// Create a private network namespace with loopback only.
    Isolated,
    /// Create a veth pair connected to a private network namespace.
    Veth,
}

impl OciNetworkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Isolated => "isolated",
            Self::Veth => "veth",
        }
    }

    pub fn into_network_mode(self) -> NetworkMode {
        match self {
            Self::Host => NetworkMode::Host,
            Self::Isolated => NetworkMode::None,
            Self::Veth => NetworkMode::Veth,
        }
    }
}

/// User namespace modes accepted by systemd-nspawn's `PrivateUsers=` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrivateUsersMode {
    Yes,
    No,
    Pick,
    Identity,
    Managed,
}

impl PrivateUsersMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Pick => "pick",
            Self::Identity => "identity",
            Self::Managed => "managed",
        }
    }
}

/// A port forwarding rule (host -> container).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host: u16,
    pub container: u16,
    pub proto: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IdmapSuffix {
    #[default]
    None,
    Noidmap,
    Idmap,
    Rootidmap,
    Owneridmap,
}

impl IdmapSuffix {
    pub fn to_index(&self) -> usize {
        match self {
            IdmapSuffix::None => 0,
            IdmapSuffix::Noidmap => 1,
            IdmapSuffix::Idmap => 2,
            IdmapSuffix::Rootidmap => 3,
            IdmapSuffix::Owneridmap => 4,
        }
    }

    pub fn from_index(idx: usize) -> Self {
        match idx {
            1 => IdmapSuffix::Noidmap,
            2 => IdmapSuffix::Idmap,
            3 => IdmapSuffix::Rootidmap,
            4 => IdmapSuffix::Owneridmap,
            _ => IdmapSuffix::None,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            IdmapSuffix::None => "None",
            IdmapSuffix::Noidmap => "noidmap",
            IdmapSuffix::Idmap => "idmap",
            IdmapSuffix::Rootidmap => "rootidmap",
            IdmapSuffix::Owneridmap => "owneridmap",
        }
    }
}

impl std::fmt::Display for IdmapSuffix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdmapSuffix::None => write!(f, ""),
            IdmapSuffix::Noidmap => write!(f, ":noidmap"),
            IdmapSuffix::Idmap => write!(f, ":idmap"),
            IdmapSuffix::Rootidmap => write!(f, ":rootidmap"),
            IdmapSuffix::Owneridmap => write!(f, ":owneridmap"),
        }
    }
}

/// A host path to bind-mount into the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
    #[serde(default)]
    pub suffix: IdmapSuffix,
}

/// User configuration to be applied after the container is bootstrapped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateUser {
    pub username: String,
    /// Explicit numeric identity requested for this account. This is used for
    /// host-session grants whose Unix permissions depend on a stable UID.
    #[serde(default)]
    pub uid: Option<u32>,
    /// If true, add to the `sudo` / `wheel` group.
    pub sudoer: bool,
    /// Login shell (e.g., /bin/bash).
    pub shell: String,
}

impl CreateUser {
    pub fn validate(&self) -> Result<()> {
        validate_login_username(&self.username)?;
        if self.uid == Some(0) {
            return Err(NspawnError::Validation(
                "Regular users cannot request uid 0".into(),
            ));
        }
        validate_login_shell(&self.shell)
    }

    pub fn login_shell(&self) -> &str {
        if self.shell.is_empty() {
            "/bin/bash"
        } else {
            self.shell.as_str()
        }
    }
}

pub fn validate_login_username(name: &str) -> Result<()> {
    if name.is_empty() {
        return validation_error("Username cannot be empty");
    }
    if name.len() > 32 {
        return validation_error("Username is too long");
    }

    let bytes = name.as_bytes();
    if !bytes.is_ascii() {
        return validation_error("Username must be ASCII");
    }

    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return validation_error("Username must start with a letter or '_'");
    }

    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            continue;
        }
        if i == bytes.len() - 1 && b == b'$' {
            continue;
        }
        return validation_error("Username contains invalid characters");
    }

    Ok(())
}

pub fn validate_login_shell(shell: &str) -> Result<()> {
    if shell.is_empty() {
        return Ok(());
    }
    if shell.trim() != shell {
        return validation_error("Login shell cannot contain leading or trailing whitespace");
    }
    if shell.len() > 255
        || shell.contains(':')
        || shell.chars().any(char::is_control)
        || !shell.bytes().all(is_safe_shell_path_byte)
    {
        return validation_error("Login shell contains invalid characters");
    }

    let path = Path::new(shell);
    if !path.is_absolute() {
        return validation_error("Login shell must be an absolute path");
    }
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(_) => {
                has_normal_component = true;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return validation_error("Login shell path must not contain relative components");
            }
        }
    }
    if !has_normal_component {
        return validation_error("Login shell must include an executable path");
    }

    Ok(())
}

pub fn validate_chpasswd_secret(label: &str, secret: &str) -> Result<()> {
    if secret.len() > 4096 {
        return Err(NspawnError::Validation(format!(
            "{label} cannot exceed 4096 bytes"
        )));
    }
    if secret.chars().any(char::is_control) {
        return Err(NspawnError::Validation(format!(
            "{label} cannot contain control characters"
        )));
    }
    Ok(())
}

fn is_safe_shell_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
}

fn validation_error<T>(message: impl Into<String>) -> Result<T> {
    Err(NspawnError::Validation(message.into()))
}

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
    pub hostname: String,
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
            hostname: Default::default(),
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
    fn test_idmap_suffix_display() {
        assert_eq!(IdmapSuffix::None.to_string(), "");
        assert_eq!(IdmapSuffix::Idmap.to_string(), ":idmap");
        assert_eq!(IdmapSuffix::Noidmap.to_string(), ":noidmap");
        assert_eq!(IdmapSuffix::Rootidmap.to_string(), ":rootidmap");
        assert_eq!(IdmapSuffix::Owneridmap.to_string(), ":owneridmap");
    }

    #[test]
    fn test_idmap_suffix_from_index() {
        assert_eq!(IdmapSuffix::from_index(0), IdmapSuffix::None);
        assert_eq!(IdmapSuffix::from_index(1), IdmapSuffix::Noidmap);
        assert_eq!(IdmapSuffix::from_index(2), IdmapSuffix::Idmap);
        assert_eq!(IdmapSuffix::from_index(3), IdmapSuffix::Rootidmap);
        assert_eq!(IdmapSuffix::from_index(4), IdmapSuffix::Owneridmap);
        assert_eq!(IdmapSuffix::from_index(99), IdmapSuffix::None);
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
    fn create_user_accepts_default_shell_and_valid_system_names() {
        let user = CreateUser {
            username: "_svc-user$".into(),
            uid: None,
            shell: String::new(),
            sudoer: false,
        };

        assert!(user.validate().is_ok());
        assert_eq!(user.login_shell(), "/bin/bash");
    }

    #[test]
    fn create_user_rejects_invalid_username_shell_and_chpasswd_records() {
        for name in ["", "1alice", "bad/name", "bad name", "bad\nname"] {
            assert!(
                validate_login_username(name).is_err(),
                "username should be rejected: {name:?}"
            );
        }

        for shell in ["bash", "/", "/bin/../bash", "/bin/ba sh", "/bin/bash\n"] {
            assert!(
                validate_login_shell(shell).is_err(),
                "shell should be rejected: {shell:?}"
            );
        }

        for secret in ["one\ntwo", "one\rtwo", "one\0two"] {
            assert!(
                validate_chpasswd_secret("password", secret).is_err(),
                "secret should be rejected: {secret:?}"
            );
        }
        assert!(validate_chpasswd_secret("password", &"x".repeat(4097)).is_err());
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

    #[test]
    fn oci_network_mode_maps_to_supported_nspawn_modes() {
        assert_eq!(OciNetworkMode::Host.into_network_mode(), NetworkMode::Host);
        assert_eq!(
            OciNetworkMode::Isolated.into_network_mode(),
            NetworkMode::None
        );
        assert_eq!(OciNetworkMode::Veth.into_network_mode(), NetworkMode::Veth);
    }

    #[test]
    fn only_veth_based_modes_use_the_default_guest_network_stack() {
        assert!(NetworkMode::Veth.uses_default_guest_network_stack());
        assert!(NetworkMode::Bridge("br0".into()).uses_default_guest_network_stack());
        assert!(!NetworkMode::Host.uses_default_guest_network_stack());
        assert!(!NetworkMode::None.uses_default_guest_network_stack());
        assert!(!NetworkMode::MacVlan("eth0".into()).uses_default_guest_network_stack());
        assert!(!NetworkMode::IpVlan("eth0".into()).uses_default_guest_network_stack());
        assert!(!NetworkMode::Interface("eth0".into()).uses_default_guest_network_stack());
    }
}
