use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU16;

#[allow(unused_imports)]
pub use crate::domain::machine::{
    GuestHostname, GuestHostnameError, MachineName, MachineNameError,
};

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
