//! Machine runtime settings shared by deployment and configuration adapters.

use crate::domain::x11::X11BindIntent;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Network configuration requested for a machine launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum NetworkMode {
    /// Share the host's network namespace.
    Host,
    /// Private network namespace with no connectivity unless manually configured.
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

    /// Whether the guest is expected to provide its default network stack.
    pub const fn uses_default_guest_network_stack(&self) -> bool {
        matches!(self, Self::Veth | Self::Bridge(_))
    }

    /// Whether the request can contain `[Network] Port=` forwarding rules.
    pub const fn supports_port_forwarding(&self) -> bool {
        matches!(self, Self::Veth | Self::Bridge(_))
    }
}

/// Validate a host network interface name before it reaches a provider
/// renderer. Linux interface names are at most fifteen bytes and reserve a
/// small set of pseudo-device names.
pub fn validate_network_interface_name(value: &str) -> Result<(), NetworkInterfaceNameError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 15
        && !matches!(value, "." | ".." | "all" | "default")
        && !bytes.iter().all(u8::is_ascii_digit)
        && bytes
            .iter()
            .all(|byte| (33..=126).contains(byte) && !matches!(*byte, b':' | b'/' | b'%'));
    if valid {
        Ok(())
    } else {
        Err(NetworkInterfaceNameError(value.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterfaceNameError(String);

impl fmt::Display for NetworkInterfaceNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid network interface name: {:?}", self.0)
    }
}

impl std::error::Error for NetworkInterfaceNameError {}

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

/// A host-to-machine port forwarding rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host: u16,
    pub container: u16,
    pub proto: String,
}

/// The suffix controlling ID mapping for a bind mount source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IdmapSuffix {
    #[default]
    None,
    Noidmap,
    Idmap,
    Rootidmap,
    Owneridmap,
}

impl fmt::Display for IdmapSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, ""),
            Self::Noidmap => write!(f, ":noidmap"),
            Self::Idmap => write!(f, ":idmap"),
            Self::Rootidmap => write!(f, ":rootidmap"),
            Self::Owneridmap => write!(f, ":owneridmap"),
        }
    }
}

/// A host path to bind-mount into a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
    #[serde(default)]
    pub suffix: IdmapSuffix,
}

/// Runtime settings for a system container, independent of how its rootfs was obtained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub name: String,
    /// Optional static hostname for the guest. An empty value uses `name`.
    #[serde(default, alias = "hostname")]
    pub guest_hostname: String,
    pub network: Option<NetworkMode>,
    pub port_forwards: Vec<PortForward>,
    pub bind_mounts: Vec<BindMount>,
    /// Explicit host X11 projection scopes selected from the Host Integration
    /// workflow. Runtime X11 ACL authorization is separate from this field.
    #[serde(default)]
    pub x11_binds: Vec<X11BindIntent>,
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
    /// Whether to enable NVIDIA GPU passthrough (JIT managed).
    pub nvidia_gpu: bool,
    /// Whether to start an init process (`Boot=yes`).
    #[serde(default = "default_boot")]
    pub boot: bool,
}

fn default_boot() -> bool {
    true
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            name: Default::default(),
            guest_hostname: Default::default(),
            network: Default::default(),
            port_forwards: Default::default(),
            bind_mounts: Default::default(),
            x11_binds: Default::default(),
            device_binds: Default::default(),
            readonly_binds: Default::default(),
            privileged: Default::default(),
            private_users: None,
            graphics_acceleration: Default::default(),
            gpu_passthrough_all: Default::default(),
            nvidia_gpu: Default::default(),
            boot: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn only_veth_and_bridge_modes_support_port_forwarding() {
        assert!(NetworkMode::Veth.supports_port_forwarding());
        assert!(NetworkMode::Bridge("br0".into()).supports_port_forwarding());
        assert!(!NetworkMode::Host.supports_port_forwarding());
        assert!(!NetworkMode::None.supports_port_forwarding());
        assert!(!NetworkMode::MacVlan("eth0".into()).supports_port_forwarding());
        assert!(!NetworkMode::IpVlan("eth0".into()).supports_port_forwarding());
        assert!(!NetworkMode::Interface("eth0".into()).supports_port_forwarding());
    }

    #[test]
    fn network_interface_names_follow_linux_constraints() {
        for interface in ["eth0", "br-test", "veth.0", "name@peer"] {
            assert!(validate_network_interface_name(interface).is_ok());
        }
        for interface in [
            "", ".", "..", "all", "default", "123", "eth 0", "eth/0", "eth:0", "eth%0", "接口0",
        ] {
            assert!(validate_network_interface_name(interface).is_err());
        }
        assert!(validate_network_interface_name(&"a".repeat(16)).is_err());
    }

    #[test]
    fn idmap_suffix_display_is_the_nspawn_wire_suffix() {
        assert_eq!(IdmapSuffix::None.to_string(), "");
        assert_eq!(IdmapSuffix::Idmap.to_string(), ":idmap");
        assert_eq!(IdmapSuffix::Noidmap.to_string(), ":noidmap");
        assert_eq!(IdmapSuffix::Rootidmap.to_string(), ":rootidmap");
        assert_eq!(IdmapSuffix::Owneridmap.to_string(), ":owneridmap");
    }
}
