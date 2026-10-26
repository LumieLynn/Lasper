//! Guest account values and validation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path};

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
