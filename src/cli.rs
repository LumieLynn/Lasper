//! Process-level command-line parsing.
//!
//! This module selects the process route and coordinates process-level shell
//! requests through the application session service. It does not construct
//! concrete host adapters.

use std::path::PathBuf;

use crate::application::sessions::{
    GuestCommand, InteractiveShellEnvironment, SessionError, SessionService, ShellOpenIntent,
    ShellTarget, ValidatedGuestUserName, WaylandShellRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};

pub(crate) struct CliOptions {
    pub(crate) want_elevation: bool,
    pub(crate) want_cli_mode: bool,
    pub(crate) is_daemon: bool,
    pub(crate) fd_sock: Option<PathBuf>,
    pub(crate) rpc_sock: Option<PathBuf>,
    pub(crate) daemon_uid: u32,
    pub(crate) daemon_pid: u32,
}

pub(crate) enum CliDispatch {
    Application(CliOptions),
    Shell(ShellCommand),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellIoMode {
    Interactive,
    Launcher,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellCommand {
    io_mode: ShellIoMode,
    target: ShellTarget,
    wayland: ShellWaylandSelection,
    command: Option<GuestCommand>,
    want_elevation: bool,
    want_cli_mode: bool,
}

impl ShellCommand {
    pub(crate) const fn io_mode(&self) -> ShellIoMode {
        self.io_mode
    }

    pub(crate) const fn permits_elevation(&self) -> bool {
        matches!(self.io_mode, ShellIoMode::Interactive)
    }

    pub(crate) const fn wants_elevation(&self) -> bool {
        self.want_elevation
    }

    pub(crate) const fn wants_cli_mode(&self) -> bool {
        self.want_cli_mode
    }

    pub(crate) fn command(&self) -> Option<&GuestCommand> {
        self.command.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ShellWaylandSelection {
    Preferred,
    Display(WaylandDisplay),
    Disabled,
}

fn print_help() {
    println!(
        "lasper {} — A TUI for managing systemd-nspawn containers.\n\n\
         USAGE:\n    lasper [FLAGS]\n    lasper [--elevate] [--cli-mode] shell [--wayland[=DISPLAY] | --no-wayland] USER@MACHINE [--] [COMMAND [ARGUMENT...]]\n    lasper [--cli-mode] launch [--wayland[=DISPLAY] | --no-wayland] USER@MACHINE [--] COMMAND [ARGUMENT...]\n\n\
         FLAGS:\n    -v, --version    Print version\n    -h, --help       Print this message\n    -e, --elevate    Use an isolated sudo daemon for privileged operations\n    -c, --cli-mode   Use runtime-state and systemd command backends\n\n\
         SESSION OPTIONS:\n    --wayland[=DISPLAY]  Use Wayland (default); optionally select a discovered display\n    --no-wayland         Open without Wayland validation or environment\n    COMMAND [ARGUMENT...] Execute an absolute guest executable with argv values\n\n\
         `shell` owns an interactive terminal and may use elevation. `launch` is for\n    Terminal=false desktop entries, always uses the caller's authority, and waits for\n    the guest command while forwarding its output.\n\n\
         CONFIGURATION:\n    Settings are read from ~/.config/lasper/lasper.toml\n    [settings] elevate = true          Use the isolated sudo daemon.\n    [settings] cli-mode = true         Disable Lasper's direct DBus backend.\n    [settings] log-buffer-lines = N    Max log lines per container (default 5000).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Dispatch process-level commands before the TUI owns the terminal.
pub(crate) fn dispatch() -> std::result::Result<CliDispatch, i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((io_mode, shell_args)) = shell_command_arguments(&args) {
        return parse_shell_command(io_mode, &shell_args)
            .map(CliDispatch::Shell)
            .map_err(|error| {
                eprintln!("lasper: {error}");
                1
            });
    }
    parse_flags(&args).map(CliDispatch::Application)
}

fn shell_command_arguments(args: &[String]) -> Option<(ShellIoMode, Vec<String>)> {
    let (shell_index, io_mode) = args.iter().enumerate().find_map(|(index, argument)| {
        let io_mode = match argument.as_str() {
            "shell" => ShellIoMode::Interactive,
            "launch" => ShellIoMode::Launcher,
            _ => return None,
        };
        Some((index, io_mode))
    })?;
    args[..shell_index]
        .iter()
        .all(|argument| matches!(argument.as_str(), "--elevate" | "-e" | "--cli-mode" | "-c"))
        .then(|| {
            (
                io_mode,
                args[..shell_index]
                    .iter()
                    .chain(&args[shell_index + 1..])
                    .cloned()
                    .collect(),
            )
        })
}

pub(crate) async fn run_shell(command: ShellCommand, sessions: &SessionService) -> i32 {
    let io_mode = command.io_mode();
    let requested_command = command.command().cloned();
    let ShellCommand {
        target, wayland, ..
    } = command;
    let wayland = match resolve_wayland_request(sessions, wayland).await {
        Ok(wayland) => wayland,
        Err(error) => {
            report_shell_error("Wayland socket selection failed", &error);
            eprintln!(
                "lasper: hint: choose another display with --wayland=DISPLAY, or use \
                 --no-wayland for a terminal-only session"
            );
            return 1;
        }
    };
    let uses_wayland = !matches!(&wayland, WaylandShellRequest::Disabled);
    let size = match io_mode {
        ShellIoMode::Interactive => {
            match crate::adapters::session::terminal_io::inherited_terminal_size() {
                Ok(size) => size,
                Err(error) => {
                    report_shell_error("cannot attach the interactive terminal", &error);
                    return 1;
                }
            }
        }
        ShellIoMode::Launcher => crate::adapters::session::terminal_io::launcher_terminal_size(),
    };
    let mut intent = ShellOpenIntent::new(
        target,
        wayland,
        InteractiveShellEnvironment::capture(),
        size,
    );
    if let Some(command) = requested_command {
        intent = intent.with_command(command);
    }
    let handle = match sessions.open_shell(intent).await {
        Ok(handle) => handle,
        Err(error) => {
            report_shell_error("failed to open selected-user shell", &error);
            if uses_wayland {
                eprintln!(
                    "lasper: hint: choose another display with --wayland=DISPLAY, or use \
                     --no-wayland for a terminal-only session"
                );
            }
            return 1;
        }
    };

    let result = match io_mode {
        ShellIoMode::Interactive => {
            crate::adapters::session::terminal_io::run_inherited_terminal(handle).await
        }
        ShellIoMode::Launcher => {
            crate::adapters::session::terminal_io::run_launcher_terminal(handle).await
        }
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let context = match io_mode {
                ShellIoMode::Interactive => "interactive shell failed",
                ShellIoMode::Launcher => "guest launch failed",
            };
            report_shell_error(context, &error);
            1
        }
    }
}

async fn resolve_wayland_request(
    sessions: &SessionService,
    selection: ShellWaylandSelection,
) -> Result<WaylandShellRequest, SessionError> {
    if selection == ShellWaylandSelection::Disabled {
        return Ok(WaylandShellRequest::Disabled);
    }

    let sockets = sessions.discover_host_wayland_sockets().await;
    let socket = select_wayland_socket(sockets, &selection).map_err(SessionError::new)?;
    Ok(WaylandShellRequest::SelectedHostDisplay(socket))
}

fn select_wayland_socket(
    mut sockets: Vec<HostWaylandSocket>,
    selection: &ShellWaylandSelection,
) -> Result<HostWaylandSocket, String> {
    if sockets.is_empty() {
        return Err("no usable host Wayland socket was discovered".into());
    }

    match selection {
        ShellWaylandSelection::Preferred => Ok(sockets.remove(0)),
        ShellWaylandSelection::Display(display) => {
            let Some(index) = sockets
                .iter()
                .position(|socket| socket.display() == display)
            else {
                let available = sockets
                    .iter()
                    .map(|socket| socket.display().as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "Wayland display {display} was not discovered (available: {available})"
                ));
            };
            Ok(sockets.remove(index))
        }
        ShellWaylandSelection::Disabled => {
            Err("internal error: disabled Wayland selection reached discovery".into())
        }
    }
}

fn report_shell_error(context: &str, error: &SessionError) {
    eprintln!("lasper: {context}: {error}");
    if let Some(hint) = error.hint() {
        eprintln!("lasper: hint: {hint}");
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

fn parse_shell_command(io_mode: ShellIoMode, args: &[String]) -> Result<ShellCommand, String> {
    let command_name = match io_mode {
        ShellIoMode::Interactive => "shell",
        ShellIoMode::Launcher => "launch",
    };
    let mut target = None;
    let mut wayland = ShellWaylandSelection::Preferred;
    let mut selection_was_explicit = false;
    let mut command_program = None;
    let mut command_args = Vec::new();
    let mut command_started = false;
    let mut want_elevation = false;
    let mut want_cli_mode = false;

    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if command_started {
            command_args.push(argument.clone());
            index += 1;
            continue;
        }

        let requested_wayland = if argument == "--wayland" {
            Some(ShellWaylandSelection::Preferred)
        } else if let Some(display) = argument.strip_prefix("--wayland=") {
            if display.is_empty() {
                return Err("--wayland= requires a display name".into());
            }
            Some(ShellWaylandSelection::Display(
                WaylandDisplay::new(display.to_string())
                    .map_err(|error| format!("invalid --wayland display: {error}"))?,
            ))
        } else if argument == "--no-wayland" {
            Some(ShellWaylandSelection::Disabled)
        } else {
            None
        };

        if let Some(requested_wayland) = requested_wayland {
            if selection_was_explicit {
                return Err(format!(
                    "{command_name} accepts only one Wayland selection option"
                ));
            }
            wayland = requested_wayland;
            selection_was_explicit = true;
            index += 1;
            continue;
        }

        match argument.as_str() {
            "--elevate" | "-e" => {
                want_elevation = true;
                index += 1;
                continue;
            }
            "--cli-mode" | "-c" => {
                want_cli_mode = true;
                index += 1;
                continue;
            }
            "--" if target.is_none() => {
                return Err(format!(
                    "{command_name} command separator must follow USER@MACHINE"
                ));
            }
            "--" => {
                command_started = true;
                index += 1;
                if index == args.len() {
                    return Err(format!(
                        "{command_name} command separator requires an executable"
                    ));
                }
                command_program = Some(args[index].clone());
                index += 1;
                continue;
            }
            _ => {}
        }

        if target.is_none() {
            if argument.starts_with('-') {
                return Err(format!("unknown {command_name} option: {argument}"));
            }
            target = Some(parse_shell_target(argument)?);
            index += 1;
            continue;
        }

        // Once the target is consumed, the first remaining value is the
        // executable and every subsequent value belongs to its argv.
        command_started = true;
        command_program = Some(argument.clone());
        index += 1;
    }

    let target = target.ok_or_else(|| format!("{command_name} requires USER@MACHINE"))?;
    let command = command_program
        .map(|program| GuestCommand::new(program, command_args))
        .transpose()
        .map_err(|error| format!("invalid guest command: {error}"))?;
    if io_mode == ShellIoMode::Launcher && want_elevation {
        return Err("launch does not support --elevate; desktop authorization uses polkit".into());
    }
    if io_mode == ShellIoMode::Launcher && command.is_none() {
        return Err("launch requires a guest executable".into());
    }
    Ok(ShellCommand {
        io_mode,
        target,
        wayland,
        command,
        want_elevation,
        want_cli_mode,
    })
}

fn parse_flags(args: &[String]) -> std::result::Result<CliOptions, i32> {
    let mut options = CliOptions {
        want_elevation: false,
        want_cli_mode: false,
        is_daemon: false,
        fd_sock: None,
        rpc_sock: None,
        daemon_uid: 0,
        daemon_pid: 0,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-v" => {
                println!("lasper {}", env!("CARGO_PKG_VERSION"));
                return Err(0);
            }
            "--help" | "-h" => {
                print_help();
                return Err(0);
            }
            "--elevate" | "-e" => options.want_elevation = true,
            "--cli-mode" | "-c" => options.want_cli_mode = true,
            "--daemon" => options.is_daemon = true,
            "--fd-sock" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --fd-sock requires a path argument");
                    return Err(1);
                }
                options.fd_sock = Some(PathBuf::from(&args[i]));
            }
            "--rpc-sock" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --rpc-sock requires a path argument");
                    return Err(1);
                }
                options.rpc_sock = Some(PathBuf::from(&args[i]));
            }
            "--daemon-uid" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --daemon-uid requires a uid argument");
                    return Err(1);
                }
                options.daemon_uid = match args[i].parse::<u32>() {
                    Ok(uid) => uid,
                    Err(_) => {
                        eprintln!("lasper: --daemon-uid must be a positive integer");
                        return Err(1);
                    }
                };
            }
            "--daemon-pid" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --daemon-pid requires a pid argument");
                    return Err(1);
                }
                options.daemon_pid = match args[i].parse::<u32>() {
                    Ok(pid) if pid > 0 => pid,
                    _ => {
                        eprintln!("lasper: --daemon-pid must be a positive integer");
                        return Err(1);
                    }
                };
            }
            other => {
                eprintln!("lasper: unknown flag: {}", other);
                return Err(1);
            }
        }
        i += 1;
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        JournalSessionHandle, JournalSessionRequest, SessionPort, TerminalSessionHandle,
        TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext,
    };
    use crate::domain::wayland::SocketRevision;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn parse_interactive(args: &[String]) -> Result<ShellCommand, String> {
        parse_shell_command(ShellIoMode::Interactive, args)
    }

    fn parse_launcher(args: &[String]) -> Result<ShellCommand, String> {
        parse_shell_command(ShellIoMode::Launcher, args)
    }

    fn socket(display: &str, inode: u64) -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new(display).unwrap(),
            PathBuf::from("/run/user/1000"),
            PathBuf::from(format!("/run/user/1000/{display}")),
            1000,
            1000,
            1000,
            0o700,
            SocketRevision {
                device: 1,
                inode,
                ctime_seconds: 1,
                ctime_nanoseconds: 0,
            },
        )
        .unwrap()
    }

    #[derive(Default)]
    struct CountingSessionPort {
        discovery_calls: AtomicUsize,
    }

    #[async_trait]
    impl SessionPort for CountingSessionPort {
        async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket> {
            self.discovery_calls.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        }

        async fn open_terminal(
            &self,
            _request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            panic!("CLI preparation must not open a terminal")
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            panic!("disabled Wayland must not run a probe")
        }

        async fn open_journal(
            &self,
            _request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            panic!("CLI preparation must not open a journal")
        }
    }

    #[test]
    fn shell_target_parses_one_validated_user_and_machine() {
        let target = parse_shell_target("1000@demo").unwrap();
        assert_eq!(target.user().as_str(), "1000");
        assert_eq!(target.machine().as_str(), "demo");
    }

    #[test]
    fn shell_target_rejects_extra_delimiters_and_option_shaped_users() {
        for target in ["demo", "@demo", "alice@", "alice@demo@other", "-root@demo"] {
            assert!(parse_shell_target(target).is_err(), "{target:?}");
        }
    }

    #[test]
    fn shell_defaults_to_the_preferred_wayland_display() {
        let command = parse_interactive(&arguments(&["alice@demo"])).unwrap();

        assert_eq!(command.io_mode(), ShellIoMode::Interactive);
        assert!(command.permits_elevation());
        assert_eq!(command.target.user().as_str(), "alice");
        assert_eq!(command.target.machine().as_str(), "demo");
        assert_eq!(command.wayland, ShellWaylandSelection::Preferred);
        assert!(command.command().is_none());
        assert!(!command.wants_elevation());
        assert!(!command.wants_cli_mode());
    }

    #[test]
    fn shell_accepts_an_exact_display_or_explicitly_disables_wayland() {
        let selected =
            parse_interactive(&arguments(&["--wayland=wayland-1", "alice@demo"])).unwrap();
        assert_eq!(
            selected.wayland,
            ShellWaylandSelection::Display(WaylandDisplay::new("wayland-1").unwrap())
        );

        let disabled = parse_interactive(&arguments(&["alice@demo", "--no-wayland"])).unwrap();
        assert_eq!(disabled.wayland, ShellWaylandSelection::Disabled);
    }

    #[test]
    fn shell_accepts_the_shared_authority_and_transport_flags() {
        let command =
            parse_interactive(&arguments(&["--elevate", "alice@demo", "--cli-mode"])).unwrap();

        assert!(command.wants_elevation());
        assert!(command.wants_cli_mode());
    }

    #[test]
    fn shell_forwards_a_guest_command_with_or_without_separator() {
        let direct = parse_interactive(&arguments(&[
            "alice@demo",
            "/usr/bin/kitty",
            "--single-instance",
        ]))
        .unwrap();
        let separated = parse_interactive(&arguments(&[
            "alice@demo",
            "--",
            "/usr/bin/kitty",
            "--single-instance",
        ]))
        .unwrap();

        assert_eq!(direct.command(), separated.command());
        let command = direct.command().expect("guest command");
        assert_eq!(command.program(), "/usr/bin/kitty");
        assert_eq!(command.args(), ["--single-instance"]);
    }

    #[test]
    fn shell_route_flags_work_before_or_after_the_subcommand() {
        let (before_mode, before) = shell_command_arguments(&arguments(&[
            "--elevate",
            "--cli-mode",
            "shell",
            "alice@demo",
        ]))
        .unwrap();
        let (after_mode, after) = shell_command_arguments(&arguments(&[
            "shell",
            "--elevate",
            "--cli-mode",
            "alice@demo",
        ]))
        .unwrap();

        assert_eq!(
            parse_shell_command(before_mode, &before).unwrap(),
            parse_shell_command(after_mode, &after).unwrap()
        );
        assert!(
            shell_command_arguments(&arguments(&["--unknown", "shell", "alice@demo"])).is_none()
        );
    }

    #[test]
    fn shell_rejects_conflicting_options_and_invalid_commands() {
        for args in [
            arguments(&["--wayland", "--no-wayland", "alice@demo"]),
            arguments(&["--wayland=", "alice@demo"]),
            arguments(&["alice@demo", "extra"]),
            arguments(&["alice@demo", "--"]),
            arguments(&["--unknown", "alice@demo"]),
        ] {
            assert!(parse_interactive(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn launch_requires_a_command_and_uses_launcher_io() {
        let command = parse_launcher(&arguments(&[
            "--cli-mode",
            "alice@demo",
            "--",
            "/usr/bin/kitty",
            "--single-instance",
        ]))
        .unwrap();

        assert_eq!(command.io_mode(), ShellIoMode::Launcher);
        assert!(!command.permits_elevation());
        assert!(command.wants_cli_mode());
        assert_eq!(command.command().unwrap().program(), "/usr/bin/kitty");
        assert_eq!(command.command().unwrap().args(), ["--single-instance"]);
        assert!(parse_launcher(&arguments(&["alice@demo"])).is_err());
    }

    #[test]
    fn launch_rejects_elevation_before_or_after_the_subcommand() {
        for args in [
            arguments(&["--elevate", "launch", "alice@demo", "/bin/true"]),
            arguments(&["--elevate", "launch", "alice@demo"]),
            arguments(&["launch", "--elevate", "alice@demo", "/bin/true"]),
        ] {
            let (mode, command_args) = shell_command_arguments(&args).unwrap();
            let error = parse_shell_command(mode, &command_args).unwrap_err();
            assert!(error.contains("does not support --elevate"), "{error}");
        }
    }

    #[test]
    fn exact_display_selection_uses_all_discovered_sockets() {
        let selected = select_wayland_socket(
            vec![socket("wayland-0", 1), socket("wayland-1", 2)],
            &ShellWaylandSelection::Display(WaylandDisplay::new("wayland-1").unwrap()),
        )
        .unwrap();

        assert_eq!(selected.display().as_str(), "wayland-1");
        assert!(select_wayland_socket(
            vec![socket("wayland-0", 1)],
            &ShellWaylandSelection::Display(WaylandDisplay::new("wayland-2").unwrap()),
        )
        .unwrap_err()
        .contains("available: wayland-0"));
    }

    #[tokio::test]
    async fn disabled_wayland_skips_discovery_and_probe() {
        let port = Arc::new(CountingSessionPort::default());
        let sessions = SessionService::new(port.clone());
        let request = resolve_wayland_request(&sessions, ShellWaylandSelection::Disabled)
            .await
            .unwrap();

        assert!(matches!(request, WaylandShellRequest::Disabled));
        assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
    }
}
