//! Validated guest executable and argument boundaries.

use std::path::Path;

const MAX_GUEST_COMMAND_ARGS: usize = 256;
const MAX_GUEST_COMMAND_ARG_BYTES: usize = 4096;
const MAX_GUEST_COMMAND_TOTAL_BYTES: usize = 64 * 1024;

/// A command to execute as the selected guest user.
///
/// The executable is deliberately required to be an absolute path because
/// machine1's `OpenMachineShell` contract has the same requirement. Arguments
/// remain separate values all the way to the runtime transport. Commands are
/// never assembled by interpolating those arguments into a shell string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestCommand {
    program: String,
    args: Vec<String>,
}

impl GuestCommand {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Result<Self, GuestCommandError> {
        let program = program.into();
        validate_guest_command_value(&program, true)?;
        if !Path::new(&program).is_absolute() {
            return Err(GuestCommandError::ProgramNotAbsolute);
        }
        if args.len() > MAX_GUEST_COMMAND_ARGS {
            return Err(GuestCommandError::TooManyArguments {
                maximum: MAX_GUEST_COMMAND_ARGS,
            });
        }

        let mut total_bytes = program.len();
        for argument in &args {
            validate_guest_command_value(argument, false)?;
            total_bytes = total_bytes
                .checked_add(argument.len())
                .and_then(|total| total.checked_add(1))
                .ok_or(GuestCommandError::TooLong {
                    maximum: MAX_GUEST_COMMAND_TOTAL_BYTES,
                })?;
        }
        if total_bytes > MAX_GUEST_COMMAND_TOTAL_BYTES {
            return Err(GuestCommandError::TooLong {
                maximum: MAX_GUEST_COMMAND_TOTAL_BYTES,
            });
        }

        Ok(Self { program, args })
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Return the complete process argument vector, including argv[0].
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

fn validate_guest_command_value(value: &str, program: bool) -> Result<(), GuestCommandError> {
    if program && value.is_empty() {
        return Err(GuestCommandError::EmptyProgram);
    }
    if value.len() > MAX_GUEST_COMMAND_ARG_BYTES {
        return Err(GuestCommandError::ArgumentTooLong {
            maximum: MAX_GUEST_COMMAND_ARG_BYTES,
        });
    }
    if value.contains('\0') {
        return Err(GuestCommandError::NulByte);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestCommandError {
    #[error("guest command executable cannot be empty")]
    EmptyProgram,
    #[error("guest command executable must be an absolute path")]
    ProgramNotAbsolute,
    #[error("guest command contains a NUL byte")]
    NulByte,
    #[error("guest command argument exceeds the {maximum}-byte limit")]
    ArgumentTooLong { maximum: usize },
    #[error("guest command has more than {maximum} arguments")]
    TooManyArguments { maximum: usize },
    #[error("guest command exceeds the {maximum}-byte total limit")]
    TooLong { maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_command_keeps_argv_boundaries_and_requires_absolute_program() {
        let command = GuestCommand::new(
            "/usr/bin/kitty",
            vec!["--class".into(), "a b".into(), String::new()],
        )
        .unwrap();
        assert_eq!(command.program(), "/usr/bin/kitty");
        assert_eq!(command.args(), ["--class", "a b", ""]);
        assert_eq!(command.argv(), ["/usr/bin/kitty", "--class", "a b", ""]);
        assert!(matches!(
            GuestCommand::new("kitty", Vec::new()),
            Err(GuestCommandError::ProgramNotAbsolute)
        ));
    }

    #[test]
    fn guest_command_rejects_nul_and_unbounded_values() {
        assert!(matches!(
            GuestCommand::new("/bin/echo\0bad", Vec::new()),
            Err(GuestCommandError::NulByte)
        ));
        assert!(matches!(
            GuestCommand::new("/bin/echo", vec!["x".repeat(4097)]),
            Err(GuestCommandError::ArgumentTooLong { .. })
        ));
    }
}
