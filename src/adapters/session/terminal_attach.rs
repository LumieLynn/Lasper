//! Build native machinectl PTY commands with an explicit terminal environment.

use crate::application::sessions::InteractiveShellEnvironment;
use crate::domain::machine::MachineName;
pub use crate::domain::session::TerminalAttachmentKind as TerminalAttachKind;
use portable_pty::CommandBuilder;

#[derive(Clone, Debug, PartialEq, Eq)]
enum AttachProcessEnvironment {
    Inherited,
    ExplicitTerminal(crate::application::sessions::InteractiveShellEnvironment),
    Dumb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalAttachCommand {
    kind: TerminalAttachKind,
    program: String,
    args: Vec<String>,
    process_environment: AttachProcessEnvironment,
}

impl TerminalAttachCommand {
    pub fn kind(&self) -> TerminalAttachKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    #[cfg(test)]
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub fn into_pty_command(self) -> CommandBuilder {
        let Self {
            program,
            args,
            process_environment,
            ..
        } = self;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        match process_environment {
            AttachProcessEnvironment::Inherited => {}
            AttachProcessEnvironment::ExplicitTerminal(environment) => {
                command.env("TERM", environment.term());
                match environment.colorterm() {
                    Some(value) => command.env("COLORTERM", value),
                    None => command.env_remove("COLORTERM"),
                }
                match environment.no_color() {
                    Some(value) => command.env("NO_COLOR", value),
                    None => command.env_remove("NO_COLOR"),
                }
            }
            AttachProcessEnvironment::Dumb => {
                // The systemd tools emit OSC 3008 context sequences whenever
                // its own terminal is non-dumb, including after probe output.
                command.env("TERM", "dumb");
                command.env_remove("COLORTERM");
                command.env_remove("NO_COLOR");
            }
        }
        command
    }

    /// Construct a command with the selected terminal capability environment.
    /// The runtime systemd tools adapter supplies the argv; this type only owns PTY
    /// process-environment semantics.
    pub(crate) fn with_terminal_environment(
        args: Vec<String>,
        environment: InteractiveShellEnvironment,
    ) -> Self {
        Self {
            kind: TerminalAttachKind::Login,
            program: "machinectl".to_string(),
            args,
            process_environment: AttachProcessEnvironment::ExplicitTerminal(environment),
        }
    }

    /// Construct a command with a dumb terminal environment for a bounded
    /// machine probe, so systemd tools context sequences cannot corrupt its protocol.
    pub(crate) fn with_dumb_environment(args: Vec<String>) -> Self {
        Self {
            kind: TerminalAttachKind::Login,
            program: "machinectl".to_string(),
            args,
            process_environment: AttachProcessEnvironment::Dumb,
        }
    }
}

pub(crate) fn login(name: &MachineName) -> TerminalAttachCommand {
    TerminalAttachCommand {
        kind: TerminalAttachKind::Login,
        program: "machinectl".to_string(),
        args: vec!["--".to_string(), "login".to_string(), name.to_string()],
        process_environment: AttachProcessEnvironment::Inherited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_prompt_uses_the_native_machinectl_command() {
        let command = login(&MachineName::new("test-machine").unwrap());
        assert_eq!(command.kind(), TerminalAttachKind::Login);
        assert_eq!(command.program(), "machinectl");
        assert_eq!(command.args(), ["--", "login", "test-machine"]);
    }
}
