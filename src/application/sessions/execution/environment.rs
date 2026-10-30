//! The terminal and display environment allowed into a guest session.

use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Component, Path};

const MAX_TERM_BYTES: usize = 256;
const MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES: usize = 4096;

/// Terminal capability variables inherited by one selected-user shell. The
/// variable names are fixed; callers cannot add arbitrary entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InteractiveShellEnvironment {
    term: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    colorterm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_color: Option<String>,
}

impl InteractiveShellEnvironment {
    /// Environment used by the embedded TUI terminal.  The TUI owns a
    /// terminal emulator rather than the user's outer terminal, so inheriting
    /// the host TERM would describe the wrong terminal capabilities.
    pub fn embedded() -> Self {
        let captured = Self::capture();
        Self {
            term: "xterm-256color".to_string(),
            colorterm: captured.colorterm,
            no_color: captured.no_color,
        }
    }

    pub fn capture() -> Self {
        let term = std::env::var("TERM")
            .ok()
            .filter(|value| valid_term(value))
            .unwrap_or_else(|| "dumb".to_string());
        let colorterm = captured_terminal_value("COLORTERM");
        let no_color = captured_terminal_value("NO_COLOR");
        Self {
            term,
            colorterm,
            no_color,
        }
    }

    pub(crate) fn new(
        term: String,
        colorterm: Option<String>,
        no_color: Option<String>,
    ) -> Result<Self, InteractiveShellEnvironmentError> {
        if !valid_term(&term) {
            return Err(InteractiveShellEnvironmentError::InvalidTerm);
        }
        for (name, value) in [("COLORTERM", &colorterm), ("NO_COLOR", &no_color)] {
            if value
                .as_deref()
                .is_some_and(|value| !valid_terminal_value(value))
            {
                return Err(InteractiveShellEnvironmentError::InvalidValue { name });
            }
        }
        Ok(Self {
            term,
            colorterm,
            no_color,
        })
    }

    pub fn term(&self) -> &str {
        &self.term
    }

    pub fn colorterm(&self) -> Option<&str> {
        self.colorterm.as_deref()
    }

    pub fn no_color(&self) -> Option<&str> {
        self.no_color.as_deref()
    }
}

impl Default for InteractiveShellEnvironment {
    fn default() -> Self {
        Self {
            term: "dumb".to_string(),
            colorterm: None,
            no_color: None,
        }
    }
}

impl<'de> Deserialize<'de> for InteractiveShellEnvironment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Environment {
            term: String,
            #[serde(default)]
            colorterm: Option<String>,
            #[serde(default)]
            no_color: Option<String>,
        }

        let environment = Environment::deserialize(deserializer)?;
        Self::new(
            environment.term,
            environment.colorterm,
            environment.no_color,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InteractiveShellEnvironmentError {
    #[error("TERM is not a valid terminal type name")]
    InvalidTerm,
    #[error("{name} contains control characters or exceeds the value limit")]
    InvalidValue { name: &'static str },
}

fn captured_terminal_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| valid_terminal_value(value))
}

fn valid_term(value: &str) -> bool {
    !value.is_empty()
        && value != "unknown"
        && value.len() <= MAX_TERM_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.'))
}

fn valid_terminal_value(value: &str) -> bool {
    value.len() <= MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES && !value.chars().any(char::is_control)
}

/// Protocol probes inherit no interactive guest variables. The tools client
/// also uses TERM=dumb so its own context sequences cannot enter probe frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuestEnvironment {
    Interactive(GuestDisplayEnvironment),
    Probe,
}

impl GuestEnvironment {
    pub(crate) fn shell(
        terminal: InteractiveShellEnvironment,
        wayland_display: Option<&Path>,
        x11_display: Option<u16>,
    ) -> Result<Self, GuestEnvironmentError> {
        GuestDisplayEnvironment::shell(terminal, wayland_display, x11_display)
            .map(Self::Interactive)
    }

    pub(crate) fn assignments(&self) -> Vec<String> {
        match self {
            Self::Interactive(environment) => environment.assignments(),
            Self::Probe => Vec::new(),
        }
    }
}

const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4096;

/// The closed environment allowlist for a selected user's shell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestDisplayEnvironment {
    terminal: InteractiveShellEnvironment,
    wayland_display: Option<String>,
    x11_display: Option<u16>,
}

impl GuestDisplayEnvironment {
    fn shell(
        terminal: InteractiveShellEnvironment,
        wayland_display: Option<&Path>,
        x11_display: Option<u16>,
    ) -> Result<Self, GuestEnvironmentError> {
        Ok(Self {
            terminal,
            wayland_display: wayland_display.map(validate_absolute_path).transpose()?,
            x11_display,
        })
    }

    fn assignments(&self) -> Vec<String> {
        let mut assignments = vec![format!("TERM={}", self.terminal.term())];
        if let Some(value) = self.terminal.colorterm() {
            assignments.push(format!("COLORTERM={value}"));
        }
        if let Some(value) = self.terminal.no_color() {
            assignments.push(format!("NO_COLOR={value}"));
        }
        if let Some(display) = &self.wayland_display {
            assignments.push(format!("WAYLAND_DISPLAY={display}"));
        }
        if let Some(display) = self.x11_display {
            assignments.push(format!("DISPLAY=:{display}"));
        }
        assignments
    }

    pub(crate) fn terminal_environment(&self) -> &InteractiveShellEnvironment {
        &self.terminal
    }
}

fn validate_absolute_path(path: &Path) -> Result<String, GuestEnvironmentError> {
    if !path.is_absolute() {
        return Err(GuestEnvironmentError::NotAbsolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(GuestEnvironmentError::RelativeComponent);
    }
    let value = path.to_str().ok_or(GuestEnvironmentError::NonUtf8)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(GuestEnvironmentError::InvalidValue);
    }
    if value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
        return Err(GuestEnvironmentError::TooLong {
            maximum: MAX_ENVIRONMENT_VALUE_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GuestEnvironmentError {
    #[error("WAYLAND_DISPLAY must be an absolute path")]
    NotAbsolute,
    #[error("WAYLAND_DISPLAY must not contain relative path components")]
    RelativeComponent,
    #[error("WAYLAND_DISPLAY is not valid UTF-8")]
    NonUtf8,
    #[error("WAYLAND_DISPLAY contains an empty or control-character value")]
    InvalidValue,
    #[error("WAYLAND_DISPLAY exceeds the {maximum}-byte environment value limit")]
    TooLong { maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_shell_environment_is_typed_and_wire_validated() {
        let environment = InteractiveShellEnvironment::new(
            "xterm-kitty".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let encoded = serde_json::to_value(&environment).unwrap();
        let decoded: InteractiveShellEnvironment = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.term(), "xterm-kitty");
        assert_eq!(decoded.colorterm(), Some("truecolor"));
        assert_eq!(decoded.no_color(), Some(""));
        assert!(InteractiveShellEnvironment::new("bad term".into(), None, None).is_err());
        assert!(
            InteractiveShellEnvironment::new("xterm".into(), Some("truecolor\n".into()), None,)
                .is_err()
        );
        assert!(
            serde_json::from_value::<InteractiveShellEnvironment>(serde_json::json!({
                "term": "xterm",
                "unexpected": "value"
            }))
            .is_err()
        );
    }

    #[test]
    fn embedded_terminal_environment_uses_tui_capabilities() {
        let environment = InteractiveShellEnvironment::embedded();
        assert_eq!(environment.term(), "xterm-256color");
    }

    #[test]
    fn shell_environment_is_typed_and_deterministic() {
        let terminal = InteractiveShellEnvironment::new(
            "xterm-256color".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let environment = GuestEnvironment::shell(
            terminal,
            Some(Path::new("/run/lasper/wayland/1000/wayland-0")),
            Some(1),
        )
        .unwrap();
        assert_eq!(
            environment.assignments(),
            [
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
                "NO_COLOR=",
                "WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-0",
                "DISPLAY=:1",
            ]
        );
        assert_eq!(
            GuestEnvironment::shell(InteractiveShellEnvironment::default(), None, None)
                .unwrap()
                .assignments(),
            ["TERM=dumb"]
        );
    }

    #[test]
    fn environment_rejects_relative_or_control_paths() {
        for path in ["wayland-0", "/run/../tmp/socket", "/run/socket\n"] {
            assert!(GuestEnvironment::shell(
                InteractiveShellEnvironment::default(),
                Some(Path::new(path)),
                None,
            )
            .is_err());
        }
    }
}
