use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    BindMount, ContainerConfig, MachineName, NetworkMode, PortForward, PrivateUsersMode,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_CONFIG_ITEMS: usize = 4096;

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
    pub wayland_socket: Option<String>,
    pub nvidia_gpu: bool,
    pub boot: bool,
}

impl NspawnConfigSpec {
    pub fn validate(&self) -> Result<()> {
        validate_text("hostname", &self.hostname, true)?;

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

        if let Some(network) = &self.network {
            match network {
                NetworkMode::Bridge(name)
                | NetworkMode::MacVlan(name)
                | NetworkMode::IpVlan(name)
                | NetworkMode::Interface(name) => {
                    validate_text("network interface", name, false)?;
                }
                NetworkMode::Host | NetworkMode::None | NetworkMode::Veth => {}
            }
        }

        for bind in &self.bind_mounts {
            validate_absolute_path("bind source", &bind.source)?;
            validate_absolute_path("bind target", &bind.target)?;
        }
        for bind in &self.device_binds {
            validate_bind_expression("device bind", bind)?;
        }
        for bind in &self.readonly_binds {
            validate_bind_expression("read-only bind", bind)?;
        }

        if let Some(socket) = &self.wayland_socket {
            validate_text("Wayland socket", socket, false)?;
            if !socket.starts_with("wayland-")
                || Path::new(socket).file_name().and_then(|name| name.to_str())
                    != Some(socket.as_str())
            {
                return Err(NspawnError::Validation(format!(
                    "Invalid Wayland socket name: {socket:?}"
                )));
            }
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
            wayland_socket: config.wayland_socket.clone(),
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
        Ok(Self {
            host: value.host,
            container: value.container,
            protocol,
        })
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

fn validate_bind_expression(label: &str, value: &str) -> Result<()> {
    validate_text(label, value, false)?;
    let source = value.split_once(':').map_or(value, |(source, _)| source);
    if !Path::new(source).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "{label} source must be absolute: {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::CreateUser;

    #[test]
    fn config_spec_excludes_provisioning_secrets() {
        let config = ContainerConfig {
            name: "test".into(),
            root_password: Some("root-secret".into()),
            users: vec![CreateUser {
                username: "alice".into(),
                password: "user-secret".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let json = serde_json::to_string(&NspawnConfigSpec::try_from(&config).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(!json.contains("root-secret"));
        assert!(!json.contains("user-secret"));
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
}
