use serde::{Deserialize, Serialize};

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
    pub password: String,
    /// If true, add to the `sudo` / `wheel` group.
    pub sudoer: bool,
    /// Login shell (e.g., /bin/bash).
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiskImageSource {
    CreateNew { size: String, fs_type: String },
    ImportExisting { path: String },
}

/// Configuration for disk image storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskImageConfig {
    pub source: DiskImageSource,
    pub use_partition_table: bool,
}

impl Default for DiskImageConfig {
    fn default() -> Self {
        Self {
            source: DiskImageSource::CreateNew {
                size: "10G".to_string(),
                fs_type: "ext4".to_string(),
            },
            use_partition_table: false,
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
    /// Explicit PrivateUsers setting. None = auto-detect, Some(true) = yes, Some(false) = no.
    pub private_users: Option<String>,
    /// Whether to enable hardware graphics acceleration (Auto-detected DRI/WSL/Mali).
    pub graphics_acceleration: bool,
    pub root_password: Option<String>,
    pub users: Vec<CreateUser>,
    /// Specific Wayland socket name (e.g., Some("wayland-0")). If None, passthrough is disabled.
    pub wayland_socket: Option<String>,
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
            root_password: Default::default(),
            users: Default::default(),
            wayland_socket: Default::default(),
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
    fn test_container_config_serde_roundtrip() {
        let cfg = ContainerConfig {
            name: "test".into(),
            nvidia_gpu: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: ContainerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, cfg2);
    }
}
