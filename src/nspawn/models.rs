//! Core data models for container configuration.

use serde::{Deserialize, Serialize};

/// Represents the network configuration for a container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NetworkMode {
    /// Share the host's network namespace.
    Host,
    /// Private network namespace with no connectivity (unless manually configured).
    None,
    /// Virtual Ethernet pair (veth).
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

impl Default for NetworkMode {
    fn default() -> Self { NetworkMode::Veth }
}

/// A port forwarding rule (host -> container).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    pub host: u16,
    pub container: u16,
    pub proto: String, // Changed from &'static str for better serializability
}

/// A host path to bind-mount into the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

/// User configuration to be applied after the container is bootstrapped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateUser {
    pub username: String,
    pub password: String,
    /// If true, add to the `sudo` / `wheel` group.
    pub sudoer: bool,
    /// Login shell (e.g., /bin/bash).
    pub shell: String,
}

/// Configuration for raw file storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawStorageConfig {
    pub size: String,
    pub fs_type: String,
    pub use_partition_table: bool,
}

impl Default for RawStorageConfig {
    fn default() -> Self {
        Self {
            size: "10G".to_string(),
            fs_type: "ext4".to_string(),
            use_partition_table: false,
        }
    }
}

/// Complete configuration for a new container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Whether to grant all capabilities (required for some hardware passthrough).
    pub full_capabilities: bool,
    pub root_password: Option<String>,
    pub users: Vec<CreateUser>,
    /// Whether to enable Wayland socket passthrough.
    pub wayland_socket: bool,
    /// Raw storage specific configuration (only used if storage type is Raw).
    pub raw_config: Option<RawStorageConfig>,
}
