//! Values that describe a provisioning request independently of its transport.
//!
//! These types are consumed by the application contract, TUI and host
//! adapters. Rendering them into `.nspawn` syntax remains an adapter concern.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path};

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

/// User account requested during rootfs provisioning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateUser {
    pub username: String,
    /// Explicit numeric identity requested for this account.
    #[serde(default)]
    pub uid: Option<u32>,
    /// Whether to add the account to the sudo/wheel group.
    pub sudoer: bool,
    /// Login shell path. Empty selects the provider default.
    pub shell: String,
}

impl CreateUser {
    pub fn validate(&self) -> Result<(), UserValidationError> {
        validate_login_username(&self.username)?;
        if self.uid == Some(0) {
            return Err(UserValidationError::RootUid);
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

/// Structured validation failures for provisioning account values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserValidationError {
    EmptyUsername,
    UsernameTooLong,
    UsernameNotAscii,
    UsernameInvalidStart,
    UsernameInvalidCharacters,
    RootUid,
    ShellWhitespace,
    ShellInvalidCharacters,
    ShellNotAbsolute,
    ShellRelativeComponent,
    ShellMissingExecutable,
}

impl fmt::Display for UserValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyUsername => "Username cannot be empty",
            Self::UsernameTooLong => "Username is too long",
            Self::UsernameNotAscii => "Username must be ASCII",
            Self::UsernameInvalidStart => "Username must start with a letter or '_'",
            Self::UsernameInvalidCharacters => "Username contains invalid characters",
            Self::RootUid => "Regular users cannot request uid 0",
            Self::ShellWhitespace => "Login shell cannot contain leading or trailing whitespace",
            Self::ShellInvalidCharacters => "Login shell contains invalid characters",
            Self::ShellNotAbsolute => "Login shell must be an absolute path",
            Self::ShellRelativeComponent => "Login shell path must not contain relative components",
            Self::ShellMissingExecutable => "Login shell must include an executable path",
        };
        f.write_str(message)
    }
}

impl std::error::Error for UserValidationError {}

pub fn validate_login_username(name: &str) -> Result<(), UserValidationError> {
    if name.is_empty() {
        return Err(UserValidationError::EmptyUsername);
    }
    if name.len() > 32 {
        return Err(UserValidationError::UsernameTooLong);
    }

    let bytes = name.as_bytes();
    if !bytes.is_ascii() {
        return Err(UserValidationError::UsernameNotAscii);
    }

    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return Err(UserValidationError::UsernameInvalidStart);
    }

    for (index, &byte) in bytes.iter().enumerate().skip(1) {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            continue;
        }
        if index == bytes.len() - 1 && byte == b'$' {
            continue;
        }
        return Err(UserValidationError::UsernameInvalidCharacters);
    }

    Ok(())
}

pub fn validate_login_shell(shell: &str) -> Result<(), UserValidationError> {
    if shell.is_empty() {
        return Ok(());
    }
    if shell.trim() != shell {
        return Err(UserValidationError::ShellWhitespace);
    }
    if shell.len() > 255
        || shell.contains(':')
        || shell.chars().any(char::is_control)
        || !shell.bytes().all(is_safe_shell_path_byte)
    {
        return Err(UserValidationError::ShellInvalidCharacters);
    }

    let path = Path::new(shell);
    if !path.is_absolute() {
        return Err(UserValidationError::ShellNotAbsolute);
    }
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(UserValidationError::ShellRelativeComponent);
            }
        }
    }
    if !has_normal_component {
        return Err(UserValidationError::ShellMissingExecutable);
    }

    Ok(())
}

fn is_safe_shell_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+')
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
    fn user_values_reject_invalid_names_and_shells() {
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
    }
}
