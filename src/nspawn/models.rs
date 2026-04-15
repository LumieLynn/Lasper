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
    fn default() -> Self {
        NetworkMode::Veth
    }
}

/// A port forwarding rule (host -> container).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host: u16,
    pub container: u16,
    pub proto: String, // Changed from &'static str for better serializability
}

/// A host path to bind-mount into the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
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
    CreateNew {
        size: String,
        fs_type: String,
    },
    ImportExisting {
        path: String,
    },
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

// ── Unified data model ────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContainerState {
    Running,
    Starting,
    Exiting,
    Off,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl Ord for ContainerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.state.cmp(&other.state).then(self.name.cmp(&other.name))
    }
}

impl PartialOrd for ContainerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A group of related properties for a machine/container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PropertyGroup {
    pub name: String,
    pub properties: std::collections::HashMap<String, String>,
}

impl PropertyGroup {
    pub fn display_priority(&self) -> u8 {
        match self.name.as_str() {
            "Machine" => 0,
            "Systemd Unit" => 1,
            "Dependencies" => 10,
            _ => 5,
        }
    }
}

pub const IMPORTANT_KEYS: &[&str] = &[
    "Name",
    "State",
    "Class",
    "Enabled",
    "IPAddresses",
    "MainPID",
    "Leader",
    "Timestamp",
    "Type",
    "ReadOnly",
    "Usage",
];

/// Strongly-typed properties for a machine/container.
#[derive(Debug, Clone, Default)]
pub struct MachineProperties {
    /// Grouped properties (e.g., "Machine", "Systemd Unit", "Dependencies").
    pub groups: Vec<PropertyGroup>,
    // Placeholders for future metrics
    #[allow(dead_code)]
    pub cpu_usage: Option<f64>,
    #[allow(dead_code)]
    pub memory_usage: Option<u64>,
}

impl MachineProperties {
    pub fn get_group_mut(&mut self, name: &str) -> &mut std::collections::HashMap<String, String> {
        if let Some(pos) = self.groups.iter().position(|g| g.name == name) {
            &mut self.groups[pos].properties
        } else {
            self.groups.push(PropertyGroup {
                name: name.to_string(),
                properties: std::collections::HashMap::new(),
            });
            &mut self.groups.last_mut().unwrap().properties
        }
    }

    pub fn insert(&mut self, group: &str, key: String, value: String) {
        self.get_group_mut(group).insert(key, value);
    }

    pub fn total_rows(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| !g.properties.is_empty())
            .map(|g| g.properties.len() + 2) // 1 header + N props + 1 spacer
            .sum()
    }

    /// Returns a filtered and ordered list of 'primary' properties for summary views.
    pub fn get_summary(&self) -> Vec<(&String, &String)> {
        let mut pairs = Vec::new();
        for group in &self.groups {
            for (k, v) in &group.properties {
                if IMPORTANT_KEYS.contains(&k.as_str()) {
                    pairs.push((k, v));
                }
            }
        }

        // Sort by the order defined in IMPORTANT_KEYS
        pairs.sort_by_key(|(k, _)| {
            IMPORTANT_KEYS
                .iter()
                .position(|&ik| ik == k.as_str())
                .unwrap_or(usize::MAX)
        });

        pairs
    }
}
