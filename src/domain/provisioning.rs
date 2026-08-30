//! Values that describe a provisioning request independently of its transport.
//!
//! These types are consumed by the application contract, TUI and host
//! adapters. Rendering them into `.nspawn` syntax remains an adapter concern.

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

/// Network modes supported by the system-scoped OCI import operation.
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

    pub const fn into_network_mode(self) -> NetworkMode {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_network_mode_maps_to_supported_network_modes() {
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
    fn idmap_suffix_display_is_the_nspawn_wire_suffix() {
        assert_eq!(IdmapSuffix::None.to_string(), "");
        assert_eq!(IdmapSuffix::Idmap.to_string(), ":idmap");
        assert_eq!(IdmapSuffix::Noidmap.to_string(), ":noidmap");
        assert_eq!(IdmapSuffix::Rootidmap.to_string(), ":rootidmap");
        assert_eq!(IdmapSuffix::Owneridmap.to_string(), ":owneridmap");
    }
}
