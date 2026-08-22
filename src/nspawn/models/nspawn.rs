use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    BindMount, ContainerConfig, MachineName, NetworkMode, PortForward, PrivateUsersMode,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_CONFIG_ITEMS: usize = 4096;

/// Explicit opt-in path for exposing every host DRM device to a container.
pub const ALL_DRM_DEVICES_PATH: &str = "/dev/dri";

/// The subset of `ContainerConfig` that is allowed to affect a `.nspawn` file.
///
/// Passwords, users, image sources, and other provisioning-only data are
/// deliberately excluded from this type and therefore from the daemon wire
/// protocol.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NspawnConfigSpec {
    pub machine: MachineName,
    pub hostname: String,
    pub network: Option<NetworkMode>,
    pub resolv_conf: Option<ResolvConfMode>,
    pub port_forwards: Vec<NspawnPortForward>,
    pub bind_mounts: Vec<BindMount>,
    pub device_binds: Vec<String>,
    pub readonly_binds: Vec<String>,
    pub privileged: bool,
    pub private_users: Option<PrivateUsersMode>,
    pub graphics_acceleration: bool,
    #[serde(default)]
    pub gpu_passthrough_all: bool,
    pub nvidia_gpu: bool,
    pub boot: bool,
}

impl NspawnConfigSpec {
    pub fn validate(&self) -> Result<()> {
        validate_nspawn_hostname(&self.hostname)?;

        let expected_resolv_conf = ResolvConfMode::for_network(self.network.as_ref());
        if self.resolv_conf != expected_resolv_conf {
            return Err(NspawnError::Validation(format!(
                "Resolver policy {:?} does not match network mode {:?}",
                self.resolv_conf, self.network
            )));
        }

        if self.private_users == Some(PrivateUsersMode::Managed)
            && !self.network.as_ref().is_some_and(NetworkMode::is_private)
        {
            return Err(NspawnError::Validation(
                "PrivateUsers=managed requires an explicit private network mode".into(),
            ));
        }

        if self.port_forwards.len() > MAX_CONFIG_ITEMS
            || self.bind_mounts.len() > MAX_CONFIG_ITEMS
            || self.device_binds.len() > MAX_CONFIG_ITEMS
            || self.readonly_binds.len() > MAX_CONFIG_ITEMS
        {
            return Err(NspawnError::Validation(
                "Too many entries in .nspawn configuration".into(),
            ));
        }

        for port_forward in &self.port_forwards {
            port_forward.validate()?;
        }

        if let Some(network) = &self.network {
            match network {
                NetworkMode::Bridge(name)
                | NetworkMode::MacVlan(name)
                | NetworkMode::IpVlan(name)
                | NetworkMode::Interface(name) => {
                    validate_nspawn_interface_name(name)?;
                }
                NetworkMode::Host | NetworkMode::None | NetworkMode::Veth => {}
            }
        }

        for bind in &self.bind_mounts {
            validate_absolute_path("bind source", &bind.source)?;
            validate_absolute_path("bind target", &bind.target)?;
        }
        for bind in &self.device_binds {
            validate_absolute_path("device bind", bind)?;
        }
        for bind in &self.readonly_binds {
            validate_absolute_path("read-only bind", bind)?;
        }

        Ok(())
    }
}

impl TryFrom<&ContainerConfig> for NspawnConfigSpec {
    type Error = NspawnError;

    fn try_from(config: &ContainerConfig) -> Result<Self> {
        let resolv_conf = ResolvConfMode::for_network(config.network.as_ref());
        let spec = Self {
            machine: MachineName::new(config.name.clone())
                .map_err(|error| NspawnError::Validation(error.to_string()))?,
            hostname: config.hostname.clone(),
            network: config.network.clone(),
            resolv_conf,
            port_forwards: config
                .port_forwards
                .iter()
                .map(NspawnPortForward::try_from)
                .collect::<Result<Vec<_>>>()?,
            bind_mounts: config.bind_mounts.clone(),
            device_binds: config.device_binds.clone(),
            readonly_binds: config.readonly_binds.clone(),
            privileged: config.privileged,
            private_users: config.private_users,
            graphics_acceleration: config.graphics_acceleration,
            gpu_passthrough_all: config.gpu_passthrough_all,
            nvidia_gpu: config.nvidia_gpu,
            boot: config.boot,
        };
        spec.validate()?;
        Ok(spec)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolvConfMode {
    Off,
    BindHost,
}

impl ResolvConfMode {
    fn for_network(network: Option<&NetworkMode>) -> Option<Self> {
        match network {
            Some(NetworkMode::Host) => Some(Self::BindHost),
            Some(_) => Some(Self::Off),
            None => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::BindHost => "bind-host",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NspawnPortForward {
    pub host: u16,
    pub container: u16,
    pub protocol: TransportProtocol,
}

impl TryFrom<&PortForward> for NspawnPortForward {
    type Error = NspawnError;

    fn try_from(value: &PortForward) -> Result<Self> {
        let protocol = match value.proto.to_ascii_lowercase().as_str() {
            "tcp" => TransportProtocol::Tcp,
            "udp" => TransportProtocol::Udp,
            protocol => {
                return Err(NspawnError::Validation(format!(
                    "Unsupported port-forward protocol: {protocol:?}"
                )));
            }
        };
        let forward = Self {
            host: value.host,
            container: value.container,
            protocol,
        };
        forward.validate()?;
        Ok(forward)
    }
}

impl NspawnPortForward {
    fn validate(&self) -> Result<()> {
        if self.host == 0 || self.container == 0 {
            return Err(NspawnError::Validation(
                "Port-forward ports must be in the range 1..=65535".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

impl TransportProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

pub(crate) fn validate_nspawn_hostname(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let valid = value.len() <= 64
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if !valid {
        return Err(NspawnError::Validation(format!(
            "Invalid hostname: {value:?}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_nspawn_interface_name(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 15
        && !matches!(value, "." | ".." | "all" | "default")
        && !bytes.iter().all(u8::is_ascii_digit)
        && bytes
            .iter()
            .all(|byte| (33..=126).contains(byte) && !matches!(*byte, b':' | b'/' | b'%'));
    if !valid {
        return Err(NspawnError::Validation(format!(
            "Invalid network interface name: {value:?}"
        )));
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.is_empty())
        || value.len() > 255
        || value.chars().any(char::is_control)
    {
        return Err(NspawnError::Validation(format!(
            "Invalid {label}: {value:?}"
        )));
    }
    Ok(())
}

fn validate_absolute_path(label: &str, value: &str) -> Result<()> {
    validate_text(label, value, false)?;
    if !Path::new(value).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "{label} must be absolute: {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::CreateUser;

    #[test]
    fn config_spec_excludes_account_execution_data() {
        let config = ContainerConfig {
            name: "test".into(),
            users: vec![CreateUser {
                username: "alice".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let json = serde_json::to_string(&NspawnConfigSpec::try_from(&config).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("root_password").is_none());
        assert!(value.get("users").is_none());
        assert!(value.get("password").is_none());
    }

    #[test]
    fn config_spec_rejects_unknown_protocol_and_relative_bind() {
        let bad_protocol = ContainerConfig {
            name: "test".into(),
            port_forwards: vec![PortForward {
                host: 8080,
                container: 80,
                proto: "sctp".into(),
            }],
            ..Default::default()
        };
        assert!(NspawnConfigSpec::try_from(&bad_protocol).is_err());

        let bad_bind = ContainerConfig {
            name: "test".into(),
            bind_mounts: vec![BindMount {
                source: "relative".into(),
                target: "/srv".into(),
                readonly: false,
                suffix: Default::default(),
            }],
            ..Default::default()
        };
        assert!(NspawnConfigSpec::try_from(&bad_bind).is_err());
    }

    #[test]
    fn config_spec_derives_and_validates_resolver_policy() {
        let host = ContainerConfig {
            name: "host-network".into(),
            network: Some(NetworkMode::Host),
            ..Default::default()
        };
        let host_spec = NspawnConfigSpec::try_from(&host).unwrap();
        assert_eq!(host_spec.resolv_conf, Some(ResolvConfMode::BindHost));

        let private = ContainerConfig {
            name: "private-network".into(),
            network: Some(NetworkMode::Veth),
            ..Default::default()
        };
        let mut private_spec = NspawnConfigSpec::try_from(&private).unwrap();
        assert_eq!(private_spec.resolv_conf, Some(ResolvConfMode::Off));

        private_spec.resolv_conf = Some(ResolvConfMode::BindHost);
        assert!(private_spec.validate().is_err());
    }

    #[test]
    fn managed_private_users_requires_systemd_private_networking() {
        for network in [None, Some(NetworkMode::Host)] {
            let config = ContainerConfig {
                name: "managed-host".into(),
                network,
                private_users: Some(PrivateUsersMode::Managed),
                ..Default::default()
            };
            assert!(matches!(
                NspawnConfigSpec::try_from(&config),
                Err(NspawnError::Validation(message))
                    if message.contains("requires an explicit private network mode")
            ));
        }

        for network in [
            NetworkMode::None,
            NetworkMode::Veth,
            NetworkMode::Bridge("br0".into()),
            NetworkMode::MacVlan("eth0".into()),
            NetworkMode::IpVlan("eth0".into()),
            NetworkMode::Interface("eth1".into()),
        ] {
            let config = ContainerConfig {
                name: "managed-private".into(),
                network: Some(network),
                private_users: Some(PrivateUsersMode::Managed),
                ..Default::default()
            };
            NspawnConfigSpec::try_from(&config).unwrap();
        }
    }

    #[test]
    fn port_forwards_reject_zero_in_typed_and_deserialized_specs() {
        for (host, container) in [(0, 80), (8080, 0)] {
            let config = ContainerConfig {
                name: "port-test".into(),
                port_forwards: vec![PortForward {
                    host,
                    container,
                    proto: "tcp".into(),
                }],
                ..Default::default()
            };
            assert!(NspawnConfigSpec::try_from(&config).is_err());
        }

        let mut spec = NspawnConfigSpec::try_from(&ContainerConfig {
            name: "wire-port-test".into(),
            ..Default::default()
        })
        .unwrap();
        spec.port_forwards.push(NspawnPortForward {
            host: 0,
            container: 80,
            protocol: TransportProtocol::Tcp,
        });
        assert!(spec.validate().is_err());
    }

    #[test]
    fn hostname_validation_matches_nspawn_hostname_rules() {
        for hostname in ["", "host", "Host-01", "host.example"] {
            assert!(validate_nspawn_hostname(hostname).is_ok(), "{hostname:?}");
        }
        assert!(validate_nspawn_hostname(&"a".repeat(64)).is_ok());

        for hostname in [
            "host name",
            "host_name",
            ".host",
            "host.",
            "host..name",
            "-host",
            "host-",
        ] {
            assert!(validate_nspawn_hostname(hostname).is_err(), "{hostname:?}");
        }
        assert!(validate_nspawn_hostname(&"a".repeat(65)).is_err());
    }

    #[test]
    fn interface_validation_matches_systemd_ifname_rules() {
        for interface in ["eth0", "br-test", "veth.0", "name@peer"] {
            assert!(
                validate_nspawn_interface_name(interface).is_ok(),
                "{interface:?}"
            );
        }
        assert!(validate_nspawn_interface_name(&"a".repeat(15)).is_ok());

        for interface in [
            "", ".", "..", "all", "default", "123", "eth 0", "eth/0", "eth:0", "eth%0", "接口0",
        ] {
            assert!(
                validate_nspawn_interface_name(interface).is_err(),
                "{interface:?}"
            );
        }
        assert!(validate_nspawn_interface_name(&"a".repeat(16)).is_err());
    }
}
