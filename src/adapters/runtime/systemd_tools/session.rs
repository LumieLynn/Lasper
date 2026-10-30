//! machinectl argv and client environment for native guest sessions.

use crate::application::sessions::execution::{
    GuestEnvironment, GuestExecutionRequest, GuestProcess,
};
use crate::domain::machine::MachineName;
use portable_pty::CommandBuilder;

pub(crate) fn machine_login_command(machine: &MachineName) -> CommandBuilder {
    let mut command = CommandBuilder::new("machinectl");
    command.args(["--", "login", machine.as_str()]);
    command
}

pub(crate) fn guest_session_command(request: &GuestExecutionRequest) -> CommandBuilder {
    let mut command = CommandBuilder::new("machinectl");
    command.arg("--quiet");
    for assignment in request.environment().assignments() {
        command.arg(format!("--setenv={assignment}"));
    }
    command.args(["--", "shell"]);
    command.arg(format!(
        "{}@{}",
        request.target().user(),
        request.target().machine()
    ));
    if let GuestProcess::Command(process) = request.process() {
        command.args(process.argv());
    }

    // The client runs in the host terminal; only --setenv assignments enter
    // the guest. Suppress the client's OSC context output for protocol probes.
    match request.environment() {
        GuestEnvironment::Interactive(environment) => {
            let terminal = environment.terminal_environment();
            command.env("TERM", terminal.term());
            match terminal.colorterm() {
                Some(value) => command.env("COLORTERM", value),
                None => command.env_remove("COLORTERM"),
            }
            match terminal.no_color() {
                Some(value) => command.env("NO_COLOR", value),
                None => command.env_remove("NO_COLOR"),
            }
        }
        GuestEnvironment::Probe => {
            command.env("TERM", "dumb");
            command.env_remove("COLORTERM");
            command.env_remove("NO_COLOR");
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        GuestCommand, GuestTarget, InteractiveShellEnvironment, ValidatedGuestUserName,
    };
    use std::ffi::OsStr;
    use std::path::Path;

    fn target() -> GuestTarget {
        GuestTarget::new(
            MachineName::new("test-machine").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        )
    }

    #[test]
    fn login_prompt_and_account_shell_use_distinct_native_commands() {
        let login = machine_login_command(target().machine());
        assert_eq!(
            login.get_argv(),
            &["machinectl", "--", "login", "test-machine"]
        );

        let shell = guest_session_command(&GuestExecutionRequest::login_shell(
            target(),
            GuestEnvironment::shell(Default::default(), None, None).unwrap(),
        ));
        assert_eq!(
            shell.get_argv(),
            &[
                "machinectl",
                "--quiet",
                "--setenv=TERM=dumb",
                "--",
                "shell",
                "alice@test-machine",
            ]
        );
    }

    #[test]
    fn explicit_command_keeps_empty_and_space_containing_arguments() {
        let request = GuestExecutionRequest::command(
            target(),
            GuestEnvironment::shell(Default::default(), None, None).unwrap(),
            GuestCommand::new(
                "/usr/bin/kitty",
                vec!["--class".into(), "a b".into(), String::new()],
            )
            .unwrap(),
        );
        assert_eq!(
            guest_session_command(&request).get_argv(),
            &[
                "machinectl",
                "--quiet",
                "--setenv=TERM=dumb",
                "--",
                "shell",
                "alice@test-machine",
                "/usr/bin/kitty",
                "--class",
                "a b",
                "",
            ]
        );
    }

    #[test]
    fn guest_environment_and_client_terminal_environment_remain_separate() {
        let terminal = InteractiveShellEnvironment::new(
            "xterm-kitty".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let request = GuestExecutionRequest::login_shell(
            target(),
            GuestEnvironment::shell(
                terminal,
                Some(Path::new("/run/lasper/wayland/1000/wayland-1")),
                Some(1),
            )
            .unwrap(),
        );
        let command = guest_session_command(&request);
        assert_eq!(
            command.get_argv(),
            &[
                "machinectl",
                "--quiet",
                "--setenv=TERM=xterm-kitty",
                "--setenv=COLORTERM=truecolor",
                "--setenv=NO_COLOR=",
                "--setenv=WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-1",
                "--setenv=DISPLAY=:1",
                "--",
                "shell",
                "alice@test-machine",
            ]
        );
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-kitty")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(command.get_env("NO_COLOR"), Some(OsStr::new("")));
        assert_eq!(
            command.get_env("DISPLAY"),
            std::env::var_os("DISPLAY").as_deref()
        );
    }

    #[test]
    fn protocol_command_suppresses_client_context_output_without_adding_guest_variables() {
        let request = GuestExecutionRequest::command(
            target(),
            GuestEnvironment::Probe,
            GuestCommand::new(
                "/bin/sh",
                vec![
                    "-c".into(),
                    "printf ready".into(),
                    "probe".into(),
                    "/mnt/a b".into(),
                ],
            )
            .unwrap(),
        );
        let command = guest_session_command(&request);
        assert_eq!(
            command.get_argv(),
            &[
                "machinectl",
                "--quiet",
                "--",
                "shell",
                "alice@test-machine",
                "/bin/sh",
                "-c",
                "printf ready",
                "probe",
                "/mnt/a b",
            ]
        );
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("dumb")));
        assert_eq!(command.get_env("COLORTERM"), None);
        assert_eq!(command.get_env("NO_COLOR"), None);
    }
}
