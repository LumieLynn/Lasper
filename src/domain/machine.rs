use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// A validated systemd machine identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MachineName(String);

impl MachineName {
    pub fn new(name: impl Into<String>) -> Result<Self, MachineNameError> {
        let name = name.into();
        if !is_valid_nspawn_hostname(&name) {
            return Err(MachineNameError(name));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Compatibility projection used by the current systemd adapters.
    pub fn systemd_nspawn_unit(&self) -> String {
        format!("systemd-nspawn@{}.service", self.0)
    }
}

impl fmt::Display for MachineName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for MachineName {
    type Error = MachineNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for MachineName {
    type Error = MachineNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for MachineName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineNameError(String);

impl fmt::Display for MachineNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid machine name {:?}: expected 1-64 ASCII hostname labels (letters, digits, '-' and '.', with no empty labels or label-edge '-')",
            self.0
        )
    }
}

impl std::error::Error for MachineNameError {}

/// The static hostname configured inside a guest root filesystem.
///
/// systemd-nspawn currently applies the same syntax to `--machine=` and
/// `--hostname=`, but the values have different ownership and lifetimes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct GuestHostname(String);

impl GuestHostname {
    pub fn new(hostname: impl Into<String>) -> Result<Self, GuestHostnameError> {
        let hostname = hostname.into();
        if !is_valid_nspawn_hostname(&hostname) {
            return Err(GuestHostnameError(hostname));
        }
        Ok(Self(hostname))
    }

    pub fn resolve(configured: &str, machine: &MachineName) -> Result<Self, GuestHostnameError> {
        if configured.is_empty() {
            Self::new(machine.as_str())
        } else {
            Self::new(configured)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for GuestHostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for GuestHostname {
    type Error = GuestHostnameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for GuestHostname {
    type Error = GuestHostnameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for GuestHostname {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestHostnameError(String);

impl fmt::Display for GuestHostnameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid guest hostname {:?}: expected 1-64 ASCII hostname labels (letters, digits, '-' and '.', with no empty labels or label-edge '-')",
            self.0
        )
    }
}

impl std::error::Error for GuestHostnameError {}

fn is_valid_nspawn_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_name_rejects_traversal_and_invalid_characters() {
        for invalid in [
            "",
            ".hidden",
            "hidden.",
            "a..b",
            "with/slash",
            "contains space",
            "with_under_score",
            "-leading",
            "trailing-",
            "a.-b",
            "a.b-",
        ] {
            assert!(MachineName::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn machine_name_deserialization_validates_the_value() {
        assert!(serde_json::from_str::<MachineName>(r#""valid-machine-1""#).is_ok());
        assert!(serde_json::from_str::<MachineName>(r#""web.frontend""#).is_ok());
        assert!(serde_json::from_str::<MachineName>(r#""../invalid""#).is_err());
    }

    #[test]
    fn machine_name_builds_systemd_nspawn_unit_name() {
        let machine = MachineName::new("valid-machine-1").unwrap();
        assert_eq!(
            machine.systemd_nspawn_unit(),
            "systemd-nspawn@valid-machine-1.service"
        );
    }

    #[test]
    fn guest_hostname_defaults_to_the_machine_identity() {
        let machine = MachineName::new("web-01.example").unwrap();

        let hostname = GuestHostname::resolve("", &machine).unwrap();

        assert_eq!(hostname.as_str(), "web-01.example");
    }

    #[test]
    fn guest_hostname_is_a_distinct_validated_identity() {
        let machine = MachineName::new("machine-name").unwrap();

        let hostname = GuestHostname::resolve("guest.example", &machine).unwrap();

        assert_eq!(hostname.as_str(), "guest.example");
        assert!(GuestHostname::new("guest_name").is_err());
        assert!(serde_json::from_str::<GuestHostname>(r#""guest_name""#).is_err());
    }
}
