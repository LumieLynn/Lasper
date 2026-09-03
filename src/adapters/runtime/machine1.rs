//! Typed arguments and results for machine1 terminal methods.
//!
//! machine1 accepts arbitrary programs, argv and environment values. This
//! boundary exposes only a login prompt, a selected user's default shell, and
//! Lasper's fixed Wayland projection probe.

use crate::application::sessions::ValidatedGuestUserName;
use crate::domain::machine::MachineName;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4096;

const WAYLAND_PROBE_SCRIPT: &str = r#"euid=
egid=
while read -r key _real effective _rest; do
    case "$key" in
        Uid:) euid=$effective ;;
        Gid:) egid=$effective ;;
    esac
done < /proc/self/status
target=UNCHECKED
if [ "$#" -gt 0 ]; then
    if [ ! -e "$1" ]; then
        target=MISSING
    elif [ ! -S "$1" ]; then
        target=NOT_SOCKET
    elif [ ! -w "$1" ]; then
        target=DENIED
    else
        target=ACCESSIBLE
    fi
fi
printf '%s\n' \
    'LASPER_WAYLAND_PROBE_V1' \
    "EUID=$euid" \
    "EGID=$egid" \
    "TARGET=$target" \
    'RESULT=READY'
"#;

/// PTY returned by a machine1 manager method.
pub(crate) struct Machine1Pty {
    pub(crate) master: OwnedFd,
}

/// Closed environment allowlist for a selected-user shell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Machine1Environment {
    wayland_display: Option<String>,
}

impl Machine1Environment {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn wayland(display: &Path) -> Result<Self, Machine1EnvironmentError> {
        Ok(Self {
            wayland_display: Some(validate_absolute_path("WAYLAND_DISPLAY", display)?),
        })
    }

    pub(crate) fn assignments(&self) -> Vec<String> {
        self.wayland_display
            .iter()
            .map(|display| format!("WAYLAND_DISPLAY={display}"))
            .collect()
    }
}

fn validate_absolute_path(
    field: &'static str,
    path: &Path,
) -> Result<String, Machine1EnvironmentError> {
    if !path.is_absolute() {
        return Err(Machine1EnvironmentError::NotAbsolute { field });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(Machine1EnvironmentError::RelativeComponent { field });
    }
    let value = path
        .to_str()
        .ok_or(Machine1EnvironmentError::NonUtf8 { field })?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(Machine1EnvironmentError::InvalidValue { field });
    }
    if value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
        return Err(Machine1EnvironmentError::TooLong {
            field,
            maximum: MAX_ENVIRONMENT_VALUE_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Machine1EnvironmentError {
    #[error("{field} must be an absolute path")]
    NotAbsolute { field: &'static str },
    #[error("{field} must not contain relative path components")]
    RelativeComponent { field: &'static str },
    #[error("{field} is not valid UTF-8")]
    NonUtf8 { field: &'static str },
    #[error("{field} contains an empty or control-character value")]
    InvalidValue { field: &'static str },
    #[error("{field} exceeds the {maximum}-byte environment value limit")]
    TooLong { field: &'static str, maximum: usize },
}

/// A selected user's default login shell. There is intentionally no caller-
/// controlled executable or argv: an empty path asks machine1 to resolve the
/// account's shell inside the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Machine1ShellRequest {
    machine: MachineName,
    user: ValidatedGuestUserName,
    environment: Machine1Environment,
}

impl Machine1ShellRequest {
    pub(crate) fn new(
        machine: MachineName,
        user: ValidatedGuestUserName,
        environment: Machine1Environment,
    ) -> Self {
        Self {
            machine,
            user,
            environment,
        }
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub(crate) fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }

    pub(crate) fn environment(&self) -> &Machine1Environment {
        &self.environment
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Machine1WaylandProbeTarget {
    IdentityOnly,
    GuestSocket(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Machine1WaylandProbeRequest {
    machine: MachineName,
    user: ValidatedGuestUserName,
    target: Machine1WaylandProbeTarget,
}

impl Machine1WaylandProbeRequest {
    pub(crate) fn identity(machine: MachineName, user: ValidatedGuestUserName) -> Self {
        Self {
            machine,
            user,
            target: Machine1WaylandProbeTarget::IdentityOnly,
        }
    }

    pub(crate) fn target(
        machine: MachineName,
        user: ValidatedGuestUserName,
        target: &Path,
    ) -> Result<Self, Machine1EnvironmentError> {
        Ok(Self {
            machine,
            user,
            target: Machine1WaylandProbeTarget::GuestSocket(validate_absolute_path(
                "Wayland probe target",
                target,
            )?),
        })
    }

    pub(crate) fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub(crate) fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }

    pub(crate) fn path(&self) -> &'static str {
        "/bin/sh"
    }

    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = vec![
            "/bin/sh".into(),
            "-c".into(),
            WAYLAND_PROBE_SCRIPT.into(),
            "lasper-wayland-probe".into(),
        ];
        if let Machine1WaylandProbeTarget::GuestSocket(target) = &self.target {
            args.push(target.clone());
        }
        args
    }
}

/// Closed set of machine1 operations used by the session adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Machine1OpenRequest {
    Shell(Machine1ShellRequest),
    WaylandProbe(Machine1WaylandProbeRequest),
}

impl Machine1OpenRequest {
    pub(crate) fn shell(request: Machine1ShellRequest) -> Self {
        Self::Shell(request)
    }

    pub(crate) fn wayland_probe(request: Machine1WaylandProbeRequest) -> Self {
        Self::WaylandProbe(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> MachineName {
        MachineName::new("demo").unwrap()
    }

    fn user() -> ValidatedGuestUserName {
        ValidatedGuestUserName::new("alice").unwrap()
    }

    #[test]
    fn wayland_environment_is_typed_and_deterministic() {
        let environment =
            Machine1Environment::wayland(Path::new("/run/lasper/wayland/1000/wayland-0")).unwrap();
        assert_eq!(
            environment.assignments(),
            ["WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-0"]
        );
        assert!(Machine1Environment::empty().assignments().is_empty());
    }

    #[test]
    fn environment_rejects_relative_or_control_paths() {
        for path in ["wayland-0", "/run/../tmp/socket", "/run/socket\n"] {
            assert!(Machine1Environment::wayland(Path::new(path)).is_err());
        }
    }

    #[test]
    fn probe_has_fixed_program_and_only_the_validated_target_argument() {
        let identity = Machine1WaylandProbeRequest::identity(machine(), user());
        assert_eq!(identity.args().len(), 4);
        assert_eq!(&identity.args()[0..2], ["/bin/sh", "-c"]);
        assert!(identity.args()[2].contains("/proc/self/status"));

        let target = Machine1WaylandProbeRequest::target(
            machine(),
            user(),
            Path::new("/run/lasper/wayland/1000/wayland-0"),
        )
        .unwrap();
        assert_eq!(
            target.args().last().unwrap(),
            "/run/lasper/wayland/1000/wayland-0"
        );
    }
}
