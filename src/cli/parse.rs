//! Parse host options until a guest executable starts its untouched argv.

use std::path::PathBuf;

use crate::application::sessions::{
    GuestCommand, ShellTarget, ValidatedGuestUserName, WaylandShellRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::WaylandDisplay;

use super::help::HelpTopic;
use super::{CliDispatch, CliOptions, DaemonOptions, ShellCommand, ShellIoMode, ShellX11Selection};

#[derive(Debug)]
pub(super) enum Invocation {
    Run(CliDispatch),
    Help(HelpTopic),
    Version,
}

pub(super) fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut options = CliOptions::default();
    let mut options_finished = false;

    for (index, argument) in args.iter().enumerate() {
        if !options_finished {
            match argument.as_str() {
                "--help" | "-h" => return Ok(Invocation::Help(HelpTopic::Application)),
                "--version" | "-v" => return Ok(Invocation::Version),
                "--" => {
                    options_finished = true;
                    continue;
                }
                "--daemon" => {
                    if options != CliOptions::default() {
                        return Err(
                            "--daemon cannot be combined with public execution options".into()
                        );
                    }
                    return parse_daemon(&args[index + 1..])
                        .map(CliDispatch::Daemon)
                        .map(Invocation::Run);
                }
                "--internal-x11-reconcile" => {
                    return parse_reconcile(&args[index + 1..], options);
                }
                _ => {}
            }
            if parse_common_option(&mut options, argument)? {
                continue;
            }
        }

        let mode = match argument.as_str() {
            "shell" => ShellIoMode::Interactive,
            "launch" => ShellIoMode::Launcher,
            _ => return Err(format!("unknown command or option: {argument}")),
        };
        return parse_session(mode, &args[index + 1..], options);
    }

    Ok(Invocation::Run(CliDispatch::Application(options)))
}

/// The same switches select authority and transport on either side of a public
/// command. Session-only switches are handled solely by parse_session.
fn parse_common_option(options: &mut CliOptions, argument: &str) -> Result<bool, String> {
    match argument {
        "--elevate" | "-e" => options.want_elevation = true,
        "--systemd-tools" | "-s" => options.want_systemd_tools = true,
        _ => {
            let Some(method) = argument.strip_prefix("--elevate=") else {
                return Ok(false);
            };
            options.elevation_method = Some(method.parse()?);
            options.want_elevation = true;
        }
    }
    Ok(true)
}

fn parse_session(
    mode: ShellIoMode,
    args: &[String],
    mut options: CliOptions,
) -> Result<Invocation, String> {
    let command_name = match mode {
        ShellIoMode::Interactive => "shell",
        ShellIoMode::Launcher => "launch",
    };
    let mut target = None;
    let mut wayland = None;
    let mut x11 = None;
    let mut command = None;
    let mut quiet = false;
    let mut options_finished = false;
    let mut args = args.iter();

    while let Some(argument) = args.next() {
        if !options_finished {
            match argument.as_str() {
                "--" => {
                    options_finished = true;
                    continue;
                }
                "--help" | "-h" => {
                    return Ok(Invocation::Help(match mode {
                        ShellIoMode::Interactive => HelpTopic::Shell,
                        ShellIoMode::Launcher => HelpTopic::Launch,
                    }));
                }
                "--quiet" => {
                    quiet = true;
                    continue;
                }
                _ => {}
            }
            if parse_common_option(&mut options, argument)? {
                continue;
            }

            if let Some(selection) = parse_wayland_option(argument)? {
                if wayland.replace(selection).is_some() {
                    return Err(format!(
                        "{command_name} accepts only one Wayland selection option"
                    ));
                }
                continue;
            }

            let selected_x11 = if argument == "--with-x11" {
                Some(ShellX11Selection::Current)
            } else if let Some(display) = argument.strip_prefix("--with-x11=") {
                Some(ShellX11Selection::Display(parse_x11_display(display)?))
            } else {
                None
            };
            if let Some(selection) = selected_x11 {
                if x11.replace(selection).is_some() {
                    return Err(format!(
                        "{command_name} accepts only one Host X11 selection option"
                    ));
                }
                continue;
            }

            if argument.starts_with('-') {
                return Err(format!("unknown {command_name} option: {argument}"));
            }
        }

        if target.is_none() {
            target = Some(parse_shell_target(argument)?);
        } else {
            // Never interpret Lasper switches in the guest's argv.
            command = Some(
                GuestCommand::new(argument.clone(), args.cloned().collect())
                    .map_err(|error| format!("invalid guest command: {error}"))?,
            );
            break;
        }
    }

    let target = target.ok_or_else(|| format!("{command_name} requires USER@MACHINE"))?;
    if mode == ShellIoMode::Launcher {
        if options.want_elevation {
            return Err(
                "launch does not support --elevate; desktop authorization uses polkit".into(),
            );
        }
        if quiet {
            return Err("launch does not support --quiet; it emits no shell notices".into());
        }
        if command.is_none() {
            return Err("launch requires a guest executable".into());
        }
    }

    Ok(Invocation::Run(CliDispatch::Shell(Box::new(
        ShellCommand {
            io_mode: mode,
            target,
            wayland: wayland.unwrap_or(WaylandShellRequest::Automatic),
            x11: x11.unwrap_or(ShellX11Selection::Disabled),
            command,
            options,
            quiet,
        },
    ))))
}

fn parse_wayland_option(argument: &str) -> Result<Option<WaylandShellRequest>, String> {
    match argument {
        "--wayland" => Ok(Some(WaylandShellRequest::Automatic)),
        "--no-wayland" => Ok(Some(WaylandShellRequest::Disabled)),
        _ => {
            let Some(display) = argument.strip_prefix("--wayland=") else {
                return Ok(None);
            };
            if display.is_empty() {
                return Err("--wayland= requires a display name".into());
            }
            WaylandDisplay::new(display.to_owned())
                .map(WaylandShellRequest::Display)
                .map(Some)
                .map_err(|error| format!("invalid --wayland display: {error}"))
        }
    }
}

fn parse_shell_target(target: &str) -> Result<ShellTarget, String> {
    let Some((user, machine)) = target.split_once('@') else {
        return Err("session target must be USER@MACHINE".into());
    };
    if machine.is_empty() || machine.contains('@') || user.contains('@') {
        return Err("session target must be USER@MACHINE".into());
    }
    let machine = MachineName::new(machine)
        .map_err(|error| format!("invalid machine in session target: {error}"))?;
    let user = ValidatedGuestUserName::new(user)
        .map_err(|error| format!("invalid user in session target: {error}"))?;
    Ok(ShellTarget::new(machine, user))
}

fn parse_x11_display(value: &str) -> Result<u16, String> {
    let Some(number) = value.strip_prefix(':') else {
        return Err("--with-x11= requires a local display in :N form".into());
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("--with-x11= requires a local display in :N form".into());
    }
    number
        .parse()
        .map_err(|_| "--with-x11 display number is outside the u16 range".into())
}

// Internal entry points do not accept public session or TUI options.
fn parse_daemon(args: &[String]) -> Result<DaemonOptions, String> {
    let mut options = DaemonOptions::default();
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--fd-sock" => {
                options.fd_sock = Some(PathBuf::from(required_value(&mut args, argument)?));
            }
            "--rpc-sock" => {
                options.rpc_sock = Some(PathBuf::from(required_value(&mut args, argument)?));
            }
            "--daemon-uid" => {
                options.daemon_uid = required_value(&mut args, argument)?
                    .parse()
                    .map_err(|_| "--daemon-uid must be a non-negative integer")?;
            }
            "--daemon-pid" => {
                options.daemon_pid = required_value(&mut args, argument)?
                    .parse::<u32>()
                    .ok()
                    .filter(|pid| *pid > 0)
                    .ok_or("--daemon-pid must be a positive integer")?;
            }
            _ => return Err(format!("unknown daemon option: {argument}")),
        }
    }
    Ok(options)
}

fn required_value<'a>(
    args: &mut std::slice::Iter<'a, String>,
    option: &str,
) -> Result<&'a str, String> {
    args.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_reconcile(args: &[String], mut options: CliOptions) -> Result<Invocation, String> {
    if options.want_elevation {
        return Err("--internal-x11-reconcile does not support --elevate".into());
    }
    for argument in args {
        match argument.as_str() {
            "-s" | "--systemd-tools" => options.want_systemd_tools = true,
            _ => return Err(format!("unknown X11 reconcile option: {argument}")),
        }
    }
    Ok(Invocation::Run(CliDispatch::X11Reconcile {
        want_systemd_tools: options.want_systemd_tools,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::elevated::ElevationMethod;

    fn arguments(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn invocation(args: &[&str]) -> Result<Invocation, String> {
        parse(&arguments(args))
    }

    fn application(args: &[&str]) -> CliOptions {
        match invocation(args).unwrap() {
            Invocation::Run(CliDispatch::Application(options)) => options,
            other => panic!("expected TUI invocation, got {other:?}"),
        }
    }

    fn session(args: &[&str]) -> ShellCommand {
        match invocation(args).unwrap() {
            Invocation::Run(CliDispatch::Shell(command)) => *command,
            other => panic!("expected session invocation, got {other:?}"),
        }
    }

    #[test]
    fn common_options_select_the_same_route_before_and_after_the_command() {
        for verb in ["shell", "launch"] {
            for option in ["-s", "--systemd-tools"] {
                assert!(application(&[option]).want_systemd_tools);
                for args in [
                    vec![option, verb, "alice@demo", "/bin/echo"],
                    vec![verb, option, "alice@demo", "/bin/echo"],
                    vec![verb, "alice@demo", option, "/bin/echo"],
                ] {
                    assert!(session(&args).wants_systemd_tools(), "{args:?}");
                }
            }
        }
        assert!(!application(&[]).want_systemd_tools);
        assert!(!session(&["shell", "alice@demo"]).wants_systemd_tools());
    }

    #[test]
    fn elevation_selection_is_shared_by_tui_and_interactive_shell() {
        for (option, method) in [
            ("-e", None),
            ("--elevate", None),
            ("--elevate=sudo", Some(ElevationMethod::Sudo)),
            ("--elevate=run0", Some(ElevationMethod::Run0)),
        ] {
            let options = application(&[option]);
            assert!(options.want_elevation);
            assert_eq!(options.elevation_method, method);
            for args in [
                vec![option, "shell", "alice@demo"],
                vec!["shell", option, "alice@demo"],
                vec!["shell", "alice@demo", option],
            ] {
                let command = session(&args);
                assert!(command.wants_elevation(), "{args:?}");
                assert_eq!(command.elevation_method(), method);
            }
        }
        let command = session(&["--elevate=run0", "shell", "-e", "alice@demo"]);
        assert_eq!(command.elevation_method(), Some(ElevationMethod::Run0));
        assert!(invocation(&["--elevate=other"]).is_err());
        assert!(invocation(&["shell", "--elevate=", "alice@demo"]).is_err());
    }

    #[test]
    fn command_help_is_selected_only_before_the_guest_executable() {
        for (verb, topic) in [("shell", HelpTopic::Shell), ("launch", HelpTopic::Launch)] {
            for flag in ["-h", "--help"] {
                for args in [
                    vec![verb, flag],
                    vec!["-s", verb, flag],
                    vec![verb, "alice@demo", flag],
                ] {
                    assert!(
                        matches!(invocation(&args), Ok(Invocation::Help(actual)) if actual == topic)
                    );
                }
                let command = session(&[verb, "alice@demo", "/bin/echo", flag]);
                assert_eq!(command.command().unwrap().args(), [flag]);
            }
        }
        assert!(matches!(
            invocation(&["--help"]),
            Ok(Invocation::Help(HelpTopic::Application))
        ));
        assert!(matches!(
            invocation(&["--help", "shell"]),
            Ok(Invocation::Help(HelpTopic::Application))
        ));
        assert!(matches!(invocation(&["-s", "-v"]), Ok(Invocation::Version)));
        assert!(invocation(&["shell", "--version"]).is_err());
    }

    #[test]
    fn optional_separator_works_on_either_side_of_the_target() {
        for verb in ["shell", "launch"] {
            let direct = session(&[verb, "alice@demo", "/usr/bin/kitty", "--single-instance"]);
            for args in [
                vec![
                    verb,
                    "alice@demo",
                    "--",
                    "/usr/bin/kitty",
                    "--single-instance",
                ],
                vec![
                    verb,
                    "--",
                    "alice@demo",
                    "/usr/bin/kitty",
                    "--single-instance",
                ],
            ] {
                assert_eq!(direct, session(&args));
            }
        }
    }

    #[test]
    fn trailing_separator_without_executable_opens_the_default_shell_only() {
        let expected = session(&["shell", "alice@demo"]);
        assert_eq!(session(&["shell", "alice@demo", "--"]), expected);
        assert_eq!(session(&["shell", "--", "alice@demo"]), expected);
        assert!(invocation(&["launch", "alice@demo", "--"])
            .unwrap_err()
            .contains("requires a guest executable"));
        assert!(invocation(&["shell", "--"])
            .unwrap_err()
            .contains("requires USER@MACHINE"));
    }

    #[test]
    fn separator_ends_host_option_parsing_rather_than_starting_a_guest_command() {
        for args in [
            vec!["shell", "--", "--quiet", "alice@demo"],
            vec!["shell", "alice@demo", "--", "--help"],
            vec!["launch", "--", "alice@demo", "--with-x11"],
        ] {
            assert!(invocation(&args).is_err(), "{args:?}");
        }
        let command = session(&[
            "shell",
            "--quiet",
            "--",
            "alice@demo",
            "/bin/echo",
            "--help",
        ]);
        assert!(command.is_quiet());
        assert_eq!(command.command().unwrap().args(), ["--help"]);
    }

    #[test]
    fn guest_arguments_remain_untouched_in_both_session_commands() {
        let guest_args = [
            "--help",
            "--",
            "-s",
            "--systemd-tools",
            "--elevate=run0",
            "--quiet",
            "--no-wayland",
            "--with-x11=:2",
            "-c",
            "--cli-mode",
            "shell",
            "launch",
            "a b",
            "",
        ];
        for verb in ["shell", "launch"] {
            let args = [verb, "alice@demo", "/bin/echo"]
                .into_iter()
                .chain(guest_args)
                .collect::<Vec<_>>();
            let command = session(&args);
            assert!(!command.wants_elevation());
            assert!(!command.wants_systemd_tools());
            assert!(!command.requests_x11());
            assert!(!command.is_quiet());
            assert_eq!(command.command().unwrap().args(), guest_args);
        }
    }

    #[test]
    fn session_only_options_must_follow_the_command_name() {
        for option in ["--quiet", "--wayland", "--no-wayland", "--with-x11"] {
            assert!(
                invocation(&[option, "shell", "alice@demo"]).is_err(),
                "{option}"
            );
        }
        for args in [
            vec!["shell", "--quiet", "alice@demo"],
            vec!["shell", "alice@demo", "--quiet"],
        ] {
            assert!(session(&args).is_quiet());
        }
    }

    #[test]
    fn launch_rejects_elevation_and_shell_notices() {
        for option in ["-e", "--elevate", "--elevate=sudo", "--elevate=run0"] {
            for args in [
                vec![option, "launch", "alice@demo", "/bin/true"],
                vec!["launch", option, "alice@demo", "/bin/true"],
                vec!["launch", "alice@demo", option],
            ] {
                assert!(
                    invocation(&args)
                        .unwrap_err()
                        .contains("does not support --elevate"),
                    "{args:?}"
                );
            }
        }
        for args in [
            vec!["launch", "--quiet", "alice@demo", "/bin/true"],
            vec!["launch", "alice@demo", "--quiet", "/bin/true"],
        ] {
            assert!(invocation(&args)
                .unwrap_err()
                .contains("does not support --quiet"));
        }
    }

    #[test]
    fn shell_defaults_to_automatic_wayland_with_fallback() {
        let command = session(&["shell", "alice@demo"]);
        assert_eq!(command.io_mode(), ShellIoMode::Interactive);
        assert!(command.permits_elevation());
        assert_eq!(command.target().user().as_str(), "alice");
        assert_eq!(command.target().machine().as_str(), "demo");
        assert_eq!(command.wayland, WaylandShellRequest::Automatic);
        assert_eq!(command.x11, ShellX11Selection::Disabled);
        assert!(command.allows_wayland_fallback());
        assert!(command.command().is_none());
        assert!(!command.wants_elevation());
        assert!(!command.is_quiet());

        assert!(session(&["shell", "--wayland", "alice@demo"]).allows_wayland_fallback());
        let disabled = session(&["shell", "alice@demo", "--no-wayland"]);
        assert_eq!(disabled.wayland, WaylandShellRequest::Disabled);
        assert!(!disabled.allows_wayland_fallback());
        let selected = session(&["shell", "--wayland=wayland-1", "alice@demo"]);
        assert_eq!(
            selected.wayland,
            WaylandShellRequest::Display(WaylandDisplay::new("wayland-1").unwrap())
        );
        assert!(!selected.allows_wayland_fallback());
    }

    #[test]
    fn launch_requires_an_executable_and_uses_launcher_io_without_fallback() {
        let command = session(&[
            "launch",
            "-s",
            "alice@demo",
            "/usr/bin/kitty",
            "--single-instance",
        ]);
        assert_eq!(command.io_mode(), ShellIoMode::Launcher);
        assert!(!command.permits_elevation());
        assert!(command.wants_systemd_tools());
        assert_eq!(command.command().unwrap().program(), "/usr/bin/kitty");
        assert_eq!(command.command().unwrap().args(), ["--single-instance"]);
        assert!(!command.allows_wayland_fallback());
        assert!(invocation(&["launch", "alice@demo"]).is_err());
    }

    #[test]
    fn host_x11_requires_explicit_current_or_exact_selection() {
        assert_eq!(
            session(&["shell", "--with-x11", "alice@demo"]).x11,
            ShellX11Selection::Current
        );
        assert_eq!(
            session(&["shell", "alice@demo", "--with-x11=:12"]).x11,
            ShellX11Selection::Display(12)
        );
        for args in [
            vec!["--with-x11=", "alice@demo"],
            vec!["--with-x11=12", "alice@demo"],
            vec!["--with-x11=:1.0", "alice@demo"],
            vec!["--with-x11=:65536", "alice@demo"],
            vec!["--with-x11", "--with-x11=:1", "alice@demo"],
        ] {
            let args = std::iter::once("shell").chain(args).collect::<Vec<_>>();
            assert!(invocation(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn shell_target_keeps_validation_for_both_user_and_machine() {
        let target = parse_shell_target("1000@demo").unwrap();
        assert_eq!(target.user().as_str(), "1000");
        assert_eq!(target.machine().as_str(), "demo");
        for target in ["demo", "@demo", "alice@", "alice@demo@other", "-root@demo"] {
            assert!(parse_shell_target(target).is_err(), "{target:?}");
        }
    }

    #[test]
    fn invalid_or_conflicting_session_options_are_rejected() {
        for args in [
            vec!["shell", "--wayland", "--no-wayland", "alice@demo"],
            vec!["shell", "--wayland=", "alice@demo"],
            vec!["shell", "alice@demo", "relative-executable"],
            vec!["shell", "--unknown", "alice@demo"],
            vec!["shell", "alice@demo", "--unknown"],
            vec!["--unknown", "shell", "alice@demo"],
        ] {
            assert!(invocation(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn removed_backend_aliases_are_not_host_options() {
        for flag in ["-c", "--cli-mode"] {
            for args in [
                vec![flag],
                vec![flag, "shell", "alice@demo"],
                vec!["shell", flag, "alice@demo"],
                vec!["launch", "alice@demo", flag, "/bin/echo"],
            ] {
                assert!(invocation(&args).is_err(), "{args:?}");
            }
        }
    }

    #[test]
    fn internal_daemon_options_select_only_the_daemon_route() {
        let parsed = invocation(&[
            "--daemon",
            "--fd-sock",
            "/run/test/fd.sock",
            "--rpc-sock",
            "/run/test/rpc.sock",
            "--daemon-uid",
            "1000",
            "--daemon-pid",
            "4321",
        ])
        .unwrap();
        let Invocation::Run(CliDispatch::Daemon(options)) = parsed else {
            panic!("expected daemon invocation, got {parsed:?}");
        };
        assert_eq!(options.fd_sock, Some("/run/test/fd.sock".into()));
        assert_eq!(options.rpc_sock, Some("/run/test/rpc.sock".into()));
        assert_eq!(options.daemon_uid, 1000);
        assert_eq!(options.daemon_pid, 4321);
        assert!(matches!(
            invocation(&["--daemon", "--fd-sock", "shell"]),
            Ok(Invocation::Run(CliDispatch::Daemon(_)))
        ));
    }

    #[test]
    fn internal_options_cannot_leak_into_public_or_other_internal_routes() {
        for args in [
            vec!["--fd-sock", "/tmp/fd.sock"],
            vec!["--daemon", "--internal-x11-reconcile"],
            vec!["--daemon", "shell", "alice@demo"],
            vec!["-e", "--daemon"],
            vec!["--daemon", "-e"],
            vec!["--daemon", "--daemon-pid"],
            vec!["--daemon", "--daemon-pid", "0"],
            vec!["--daemon", "--daemon-uid", "-1"],
            vec!["shell", "--daemon", "alice@demo"],
        ] {
            assert!(invocation(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn internal_x11_reconcile_keeps_its_transport_selection() {
        for args in [
            vec!["--internal-x11-reconcile"],
            vec!["-s", "--internal-x11-reconcile"],
            vec!["--internal-x11-reconcile", "--systemd-tools"],
        ] {
            let expected_tools = args.len() > 1;
            assert!(matches!(
                invocation(&args),
                Ok(Invocation::Run(CliDispatch::X11Reconcile { want_systemd_tools }))
                    if want_systemd_tools == expected_tools
            ));
        }
        assert!(invocation(&["--internal-x11-reconcile", "--quiet"]).is_err());
        assert!(invocation(&["-e", "--internal-x11-reconcile"]).is_err());
    }

    #[test]
    fn root_separator_does_not_consume_a_subcommands_own_options() {
        assert_eq!(application(&["--"]), CliOptions::default());
        assert!(session(&["--", "shell", "--quiet", "alice@demo"]).is_quiet());
        assert!(invocation(&["--", "-s"]).is_err());
    }
}
