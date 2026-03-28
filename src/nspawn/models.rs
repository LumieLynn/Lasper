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

// ── Unified data model ────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerState {
    Running,
    Off,
    #[allow(dead_code)]
    Starting,
    #[allow(dead_code)]
    Exiting,
}

impl ContainerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Off => "poweroff",
            Self::Starting => "starting",
            Self::Exiting => "exiting",
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running | Self::Starting | Self::Exiting)
    }
}

/// A container known to machinectl — either running, poweroff, or both.
#[derive(Debug, Clone)]
pub struct ContainerEntry {
    /// The name used by machinectl
    pub name: String,
    /// Current lifecycle state
    pub state: ContainerState,
    /// Image type ("directory", "raw", "tar", …) — from list-images, None if only seen running
    pub image_type: Option<String>,
    /// Whether the image is read-only (from list-images)
    pub readonly: bool,
    /// Disk usage string (from list-images)
    pub usage: Option<String>,
    /// Network address (from list, only when running)
    pub address: Option<String>,
    /// All network addresses
    pub all_addresses: Vec<String>,
}

/// Strongly-typed properties for a machine/container.
#[derive(Debug, Clone, Default)]
pub struct MachineProperties {
    /// All raw properties returned by systemd DBus / machinectl show.
    pub properties: std::collections::HashMap<String, String>,
    // Placeholders for future metrics
    #[allow(dead_code)]
    pub cpu_usage: Option<f64>,
    #[allow(dead_code)]
    pub memory_usage: Option<u64>,
}

