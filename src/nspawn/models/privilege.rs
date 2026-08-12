use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::num::NonZeroU16;

/// A validated systemd machine name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MachineName(String);

impl MachineName {
    pub fn new(name: impl Into<String>) -> Result<Self, MachineNameError> {
        let name = name.into();
        let valid = !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid {
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

/// Signals that Lasper may send through a machine-management backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowedSignal {
    #[serde(rename = "SIGTERM")]
    Terminate,
    #[serde(rename = "SIGKILL")]
    Kill,
}

impl AllowedSignal {
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }

    pub fn as_raw(self) -> i32 {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

/// Initial dimensions for a daemon-created PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTerminalSize", into = "RawTerminalSize")]
pub struct TerminalSize {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

impl TerminalSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, TerminalSizeError> {
        let cols = NonZeroU16::new(cols).ok_or(TerminalSizeError)?;
        let rows = NonZeroU16::new(rows).ok_or(TerminalSizeError)?;
        Ok(Self { cols, rows })
    }

    pub fn cols(self) -> u16 {
        self.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.rows.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTerminalSize {
    cols: u16,
    rows: u16,
}

impl TryFrom<RawTerminalSize> for TerminalSize {
    type Error = TerminalSizeError;

    fn try_from(value: RawTerminalSize) -> Result<Self, Self::Error> {
        Self::new(value.cols, value.rows)
    }
}

impl From<TerminalSize> for RawTerminalSize {
    fn from(value: TerminalSize) -> Self {
        Self {
            cols: value.cols(),
            rows: value.rows(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSizeError;

impl fmt::Display for TerminalSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("terminal dimensions must be non-zero u16 values")
    }
}

impl std::error::Error for TerminalSizeError {}

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
    fn allowed_signal_uses_explicit_wire_names() {
        assert_eq!(
            serde_json::to_string(&AllowedSignal::Kill).unwrap(),
            r#""SIGKILL""#
        );
        assert_eq!(AllowedSignal::Terminate.as_raw(), libc::SIGTERM);
        assert_eq!(AllowedSignal::Kill.as_raw(), libc::SIGKILL);
        assert!(serde_json::from_str::<AllowedSignal>(r#""SIGUSR1""#).is_err());
    }

    #[test]
    fn terminal_size_rejects_zero_and_out_of_range_values() {
        assert!(TerminalSize::new(80, 24).is_ok());
        assert!(TerminalSize::new(0, 24).is_err());
        assert!(serde_json::from_str::<TerminalSize>(r#"{"cols":80,"rows":0}"#).is_err());
        assert!(serde_json::from_str::<TerminalSize>(r#"{"cols":65536,"rows":24}"#).is_err());
    }
}
