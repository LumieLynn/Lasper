//! Process-level command-line parsing.
//!
//! This module selects the process route and coordinates process-level shell
//! requests through the application session service. It does not construct
//! concrete host adapters.

use std::fmt;
use std::path::PathBuf;

use crate::application::sessions::{
    GuestCommand, InteractiveShellEnvironment, SessionError, SessionService, ShellOpenError,
    ShellOpenIntent, ShellTarget, TerminalSessionHandle, ValidatedGuestUserName,
    WaylandShellRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::{HostWaylandSocket, WaylandDisplay};

const WAYLAND_FALLBACK_NOTICE: &str = "🪐 Continuing without Wayland...";

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
    allow_wayland_fallback: bool,
    want_elevation: bool,
    want_cli_mode: bool,
    quiet: bool,
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

    pub(crate) const fn allows_wayland_fallback(&self) -> bool {
        self.allow_wayland_fallback
    }

    pub(crate) const fn is_quiet(&self) -> bool {
        self.quiet
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ShellWaylandSelection {
    Automatic,
    Display(WaylandDisplay),
    Disabled,
}

struct HelpPage<'a> {
    heading: String,
    usage: Vec<&'a str>,
    sections: Vec<HelpSection<'a>>,
}

impl<'a> HelpPage<'a> {
    fn new(heading: String) -> Self {
        Self {
            heading,
            usage: Vec::new(),
            sections: Vec::new(),
        }
    }

    fn usage(mut self, syntax: &'a str) -> Self {
        self.usage.push(syntax);
        self
    }

    fn section(mut self, section: HelpSection<'a>) -> Self {
        self.sections.push(section);
        self
    }
}

impl fmt::Display for HelpPage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{}", self.heading)?;
        writeln!(formatter)?;
        writeln!(formatter, "USAGE:")?;
        for syntax in &self.usage {
            writeln!(formatter, "    {syntax}")?;
        }
        for section in &self.sections {
            writeln!(formatter)?;
            section.fmt(formatter)?;
        }
        Ok(())
    }
}

struct HelpSection<'a> {
    heading: &'a str,
    items: Vec<HelpItem<'a>>,
}

enum HelpItem<'a> {
    Entry(&'a str, &'a str),
    Paragraph(&'a str),
}

impl<'a> HelpSection<'a> {
    fn new(heading: &'a str) -> Self {
        Self {
            heading,
            items: Vec::new(),
        }
    }

    fn entry(mut self, syntax: &'a str, description: &'a str) -> Self {
        self.items.push(HelpItem::Entry(syntax, description));
        self
    }

    fn paragraph(mut self, text: &'a str) -> Self {
        self.items.push(HelpItem::Paragraph(text));
        self
    }
}

impl fmt::Display for HelpSection<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{}:", self.heading)?;
        let syntax_width = self
            .items
            .iter()
            .filter_map(|item| match item {
                HelpItem::Entry(syntax, _) => Some(syntax.len()),
                HelpItem::Paragraph(_) => None,
            })
            .max()
            .unwrap_or(0);
        for (index, item) in self.items.iter().enumerate() {
            match item {
                HelpItem::Entry(syntax, description) => writeln!(
                    formatter,
                    "    {syntax:<syntax_width$}  {description}",
                    syntax_width = syntax_width
                )?,
                HelpItem::Paragraph(paragraph) => {
                    if index > 0 {
                        writeln!(formatter)?;
                    }
                    write_wrapped(formatter, paragraph, 4, 80)?;
                }
            }
        }
        Ok(())
    }
}

fn write_wrapped(
    formatter: &mut fmt::Formatter<'_>,
    text: &str,
    indentation: usize,
    width: usize,
) -> fmt::Result {
    let prefix = " ".repeat(indentation);
    let line_width = width.saturating_sub(indentation).max(1);
    let mut line = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!line.is_empty());
        if !line.is_empty() && line.len() + separator + word.len() > line_width {
            writeln!(formatter, "{prefix}{line}")?;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        writeln!(formatter, "{prefix}{line}")?;
    }
    Ok(())
}

fn help_text() -> String {
    HelpPage::new(format!(
        "lasper {} - A TUI for managing systemd-nspawn containers.",
        env!("CARGO_PKG_VERSION")
    ))
    .usage("lasper [FLAGS]")
    .usage(
        "lasper [--elevate] [--cli-mode] shell [--quiet] [--wayland[=DISPLAY] | --no-wayland] USER@MACHINE [--] [COMMAND [ARGUMENT...]]",
    )
    .usage(
        "lasper [--cli-mode] launch [--wayland[=DISPLAY] | --no-wayland] USER@MACHINE [--] COMMAND [ARGUMENT...]",
    )
    .section(
        HelpSection::new("FLAGS")
            .entry("-v, --version", "Print version")
            .entry("-h, --help", "Print this message")
            .entry(
                "-e, --elevate",
                "Use an isolated sudo daemon for privileged operations",
            )
            .entry(
                "-c, --cli-mode",
                "Use runtime-state and systemd command backends",
            ),
    )
    .section(
        HelpSection::new("SESSION OPTIONS")
            .entry(
                "--wayland[=DISPLAY]",
                "Auto-detect a configured current display; optionally select one explicitly",
            )
            .entry(
                "--no-wayland",
                "Open without Wayland validation or environment",
            )
            .entry(
                "--quiet",
                "Suppress Wayland fallback and detach notices",
            )
            .entry(
                "COMMAND [ARGUMENT...]",
                "Execute an absolute guest executable with argv values",
            )
            .paragraph(
                "`shell` owns an interactive terminal and may use elevation. Press Ctrl+] three times within one second to detach. Its automatic Wayland selection falls back to a terminal-only shell when validation fails. An exact --wayland=DISPLAY selection does not fall back.",
            )
            .paragraph(
                "`launch` is for Terminal=false desktop entries, always uses the caller's authority, and waits for the guest command while forwarding its output.",
            ),
    )
    .section(
        HelpSection::new("CONFIGURATION")
            .paragraph("Settings are read from ~/.config/lasper/lasper.toml")
            .entry(
                "[settings] elevate = true",
                "Use the isolated sudo daemon.",
            )
            .entry(
                "[settings] cli-mode = true",
                "Disable Lasper's direct DBus backend.",
            )
            .entry(
                "[settings] log-buffer-lines = N",
                "Max log lines per container (default 5000).",
            ),
    )
    .to_string()
}

fn print_help() {
    print!("{}", help_text());
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
    let allow_wayland_fallback = command.allows_wayland_fallback();
    let quiet = command.is_quiet();
    let ShellCommand {
        target, wayland, ..
    } = command;
    let (wayland, discovery_fallback) =
        match resolve_wayland_request(sessions, &target, wayland).await {
            Ok(wayland) => (wayland, None),
            Err(error) if allow_wayland_fallback => (
                WaylandShellRequest::Disabled,
                Some(WaylandFallbackCause::SocketSelection(error)),
            ),
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
    let (handle, probe_fallback) = match open_shell_with_wayland_fallback(
        sessions,
        intent,
        allow_wayland_fallback && uses_wayland,
    )
    .await
    {
        Ok(handle) => handle,
        Err(ShellAttemptError::Initial(error)) => {
            if let Some(cause) = &discovery_fallback {
                report_wayland_fallback_failure(cause, &error);
            } else {
                report_shell_error("failed to open selected-user shell", error.session_error());
                if uses_wayland && !allow_wayland_fallback {
                    eprintln!(
                        "lasper: hint: choose another display with --wayland=DISPLAY, or use \
                         --no-wayland for a terminal-only session"
                    );
                }
            }
            return 1;
        }
        Err(ShellAttemptError::Fallback { cause, error }) => {
            report_wayland_fallback_failure(&cause, &error);
            return 1;
        }
    };

    if let Some(notice) = wayland_fallback_notice(
        quiet,
        discovery_fallback.is_some() || probe_fallback.is_some(),
    ) {
        eprintln!("{notice}");
    }

    let result = match io_mode {
        ShellIoMode::Interactive => {
            crate::adapters::session::terminal_io::run_inherited_terminal(handle, !quiet).await
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

#[derive(Debug)]
enum ShellAttemptError {
    Initial(ShellOpenError),
    Fallback {
        cause: WaylandFallbackCause,
        error: ShellOpenError,
    },
}

#[derive(Debug)]
enum WaylandFallbackCause {
    SocketSelection(SessionError),
    Validation(SessionError),
}

impl WaylandFallbackCause {
    fn context(&self) -> &'static str {
        match self {
            Self::SocketSelection(_) => "Wayland socket selection failed",
            Self::Validation(_) => "Wayland validation failed",
        }
    }

    fn error(&self) -> &SessionError {
        match self {
            Self::SocketSelection(error) | Self::Validation(error) => error,
        }
    }
}

async fn open_shell_with_wayland_fallback(
    sessions: &SessionService,
    intent: ShellOpenIntent,
    allow_fallback: bool,
) -> Result<(TerminalSessionHandle, Option<WaylandFallbackCause>), ShellAttemptError> {
    match sessions.open_shell(intent.clone()).await {
        Ok(handle) => Ok((handle, None)),
        Err(ShellOpenError::WaylandPreparation(error)) if allow_fallback => {
            let cause = WaylandFallbackCause::Validation(error);
            match sessions
                .open_shell(intent.with_wayland(WaylandShellRequest::Disabled))
                .await
            {
                Ok(handle) => Ok((handle, Some(cause))),
                Err(error) => Err(ShellAttemptError::Fallback { cause, error }),
            }
        }
        Err(error) => Err(ShellAttemptError::Initial(error)),
    }
}

async fn resolve_wayland_request(
    sessions: &SessionService,
    target: &ShellTarget,
    selection: ShellWaylandSelection,
) -> Result<WaylandShellRequest, SessionError> {
    if selection == ShellWaylandSelection::Disabled {
        return Ok(WaylandShellRequest::Disabled);
    }
    if selection == ShellWaylandSelection::Automatic {
        return sessions.automatic_wayland(target.machine()).await;
    }

    let sockets = sessions.discover_host_wayland_sockets().await;
    let ShellWaylandSelection::Display(display) = selection else {
        unreachable!("disabled and automatic Wayland selections returned before discovery")
    };
    let socket = select_wayland_socket(sockets, &display).map_err(SessionError::new)?;
    Ok(WaylandShellRequest::SelectedHostDisplay(socket))
}

fn select_wayland_socket(
    mut sockets: Vec<HostWaylandSocket>,
    display: &WaylandDisplay,
) -> Result<HostWaylandSocket, String> {
    if sockets.is_empty() {
        return Err("no usable host Wayland socket was discovered".into());
    }

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

fn report_shell_error(context: &str, error: &SessionError) {
    eprintln!("lasper: {context}: {error}");
    if let Some(hint) = error.hint() {
        eprintln!("lasper: hint: {hint}");
    }
}

fn report_wayland_fallback_failure(cause: &WaylandFallbackCause, error: &ShellOpenError) {
    report_shell_error(cause.context(), cause.error());
    report_shell_error(
        "failed to open selected-user shell without Wayland",
        error.session_error(),
    );
}

fn wayland_fallback_notice(quiet: bool, fallback_used: bool) -> Option<&'static str> {
    (!quiet && fallback_used).then_some(WAYLAND_FALLBACK_NOTICE)
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
    let mut wayland = ShellWaylandSelection::Automatic;
    let mut selection_was_explicit = false;
    let mut command_program = None;
    let mut command_args = Vec::new();
    let mut command_started = false;
    let mut want_elevation = false;
    let mut want_cli_mode = false;
    let mut quiet = false;

    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if command_started {
            command_args.push(argument.clone());
            index += 1;
            continue;
        }

        let requested_wayland = if argument == "--wayland" {
            Some(ShellWaylandSelection::Automatic)
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
            "--quiet" => {
                quiet = true;
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
    if io_mode == ShellIoMode::Launcher && quiet {
        return Err("launch does not support --quiet; it emits no shell notices".into());
    }
    if io_mode == ShellIoMode::Launcher && command.is_none() {
        return Err("launch requires a guest executable".into());
    }
    let allow_wayland_fallback =
        io_mode == ShellIoMode::Interactive && wayland == ShellWaylandSelection::Automatic;
    Ok(ShellCommand {
        io_mode,
        target,
        wayland,
        command,
        allow_wayland_fallback,
        want_elevation,
        want_cli_mode,
        quiet,
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
        journal_session_channel, terminal_session_channel, JournalSessionHandle,
        JournalSessionRequest, SessionPort, TerminalLaunch, TerminalSessionHandle,
        TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext,
    };
    use crate::domain::session::TerminalAttachmentKind;
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
        automatic_calls: AtomicUsize,
        discovery_calls: AtomicUsize,
    }

    #[async_trait]
    impl SessionPort for CountingSessionPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<HostWaylandSocket>, SessionError> {
            self.automatic_calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }

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

    struct FallbackSessionPort {
        prepare_calls: AtomicUsize,
        open_calls: AtomicUsize,
        open_wayland: parking_lot::Mutex<Vec<bool>>,
        fail_fallback: bool,
    }

    impl FallbackSessionPort {
        fn new(fail_fallback: bool) -> Self {
            Self {
                prepare_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
                open_wayland: parking_lot::Mutex::new(Vec::new()),
                fail_fallback,
            }
        }
    }

    #[async_trait]
    impl SessionPort for FallbackSessionPort {
        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<HostWaylandSocket>, SessionError> {
            Ok(Some(socket("wayland-0", 1)))
        }

        async fn discover_host_wayland_sockets(&self) -> Vec<HostWaylandSocket> {
            vec![socket("wayland-0", 1)]
        }

        async fn open_terminal(
            &self,
            request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            let has_wayland = matches!(
                &request.launch,
                TerminalLaunch::SelectedUserShell { environment, .. }
                    if environment.wayland_context().is_some()
            );
            self.open_wayland.lock().push(has_wayland);
            if self.fail_fallback && !has_wayland {
                return Err(SessionError::new("simulated terminal open failure"));
            }
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            self.prepare_calls.fetch_add(1, Ordering::Relaxed);
            Err(SessionError::with_hint(
                "simulated Wayland probe failure",
                "simulated probe hint",
            ))
        }

        async fn open_journal(
            &self,
            request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            Ok(journal_session_channel(request.id).0)
        }
    }

    fn selected_wayland_intent() -> ShellOpenIntent {
        ShellOpenIntent::new(
            ShellTarget::new(
                MachineName::new("demo").unwrap(),
                ValidatedGuestUserName::new("alice").unwrap(),
            ),
            WaylandShellRequest::SelectedHostDisplay(socket("wayland-0", 1)),
            InteractiveShellEnvironment::default(),
            crate::domain::session::SessionSize::new(80, 24).unwrap(),
        )
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
    fn shell_defaults_to_automatic_wayland() {
        let command = parse_interactive(&arguments(&["alice@demo"])).unwrap();

        assert_eq!(command.io_mode(), ShellIoMode::Interactive);
        assert!(command.permits_elevation());
        assert_eq!(command.target.user().as_str(), "alice");
        assert_eq!(command.target.machine().as_str(), "demo");
        assert_eq!(command.wayland, ShellWaylandSelection::Automatic);
        assert!(command.allows_wayland_fallback());
        assert!(command.command().is_none());
        assert!(!command.wants_elevation());
        assert!(!command.wants_cli_mode());
        assert!(!command.is_quiet());
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
        assert!(!disabled.allows_wayland_fallback());

        let automatic = parse_interactive(&arguments(&["--wayland", "alice@demo"])).unwrap();
        assert!(automatic.allows_wayland_fallback());

        assert!(!selected.allows_wayland_fallback());
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
    fn quiet_is_a_right_side_shell_option() {
        assert!(shell_command_arguments(&arguments(&["--quiet", "shell", "alice@demo"])).is_none());

        for args in [
            arguments(&["shell", "--quiet", "alice@demo"]),
            arguments(&["shell", "alice@demo", "--quiet"]),
        ] {
            let (mode, command_args) = shell_command_arguments(&args).unwrap();
            assert!(parse_shell_command(mode, &command_args).unwrap().is_quiet());
        }
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

    #[tokio::test]
    async fn interactive_wayland_probe_retries_once_without_wayland() {
        let port = Arc::new(FallbackSessionPort::new(false));
        let service = SessionService::new(port.clone());
        let (mut handle, used_fallback) =
            open_shell_with_wayland_fallback(&service, selected_wayland_intent(), true)
                .await
                .unwrap();

        assert!(matches!(
            used_fallback,
            Some(WaylandFallbackCause::Validation(_))
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*port.open_wayland.lock(), [false]);
        handle.close();
    }

    #[tokio::test]
    async fn explicit_wayland_failure_does_not_retry() {
        let port = Arc::new(FallbackSessionPort::new(false));
        let service = SessionService::new(port.clone());
        let error = match open_shell_with_wayland_fallback(
            &service,
            selected_wayland_intent(),
            false,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("explicit Wayland failure unexpectedly opened a shell"),
        };

        assert!(matches!(
            error,
            ShellAttemptError::Initial(ShellOpenError::WaylandPreparation(_))
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn wayland_fallback_failure_is_reported_as_fallback_error() {
        let port = Arc::new(FallbackSessionPort::new(true));
        let service = SessionService::new(port.clone());
        let error =
            match open_shell_with_wayland_fallback(&service, selected_wayland_intent(), true).await
            {
                Err(error) => error,
                Ok(_) => panic!("Wayland fallback unexpectedly succeeded"),
            };

        assert!(matches!(
            error,
            ShellAttemptError::Fallback {
                cause: WaylandFallbackCause::Validation(_),
                error: ShellOpenError::Terminal(_),
            }
        ));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn quiet_suppresses_the_successful_wayland_fallback_notice() {
        assert_eq!(
            wayland_fallback_notice(false, true),
            Some(WAYLAND_FALLBACK_NOTICE)
        );
        assert_eq!(wayland_fallback_notice(true, true), None);
        assert_eq!(wayland_fallback_notice(false, false), None);
    }

    #[test]
    fn help_is_composed_from_sections() {
        let help = help_text();
        assert!(help.contains("USAGE:\n"));
        assert!(help.contains("FLAGS:\n"));
        assert!(help.contains("SESSION OPTIONS:\n"));
        assert!(help.contains("CONFIGURATION:\n"));
        let normalized = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains("Press Ctrl+] three times within one second to detach."));
        assert!(normalized.contains("Wayland selection falls back"));
        assert!(normalized.contains("--quiet Suppress Wayland fallback and detach notices"));
        assert_eq!(WAYLAND_FALLBACK_NOTICE, "🪐 Continuing without Wayland...");
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
    fn launch_rejects_the_shell_only_quiet_option() {
        for args in [
            arguments(&["launch", "--quiet", "alice@demo", "/bin/true"]),
            arguments(&["launch", "alice@demo", "--quiet", "/bin/true"]),
        ] {
            let (mode, command_args) = shell_command_arguments(&args).unwrap();
            let error = parse_shell_command(mode, &command_args).unwrap_err();
            assert!(error.contains("does not support --quiet"), "{error}");
        }
    }

    #[test]
    fn exact_display_selection_uses_all_discovered_sockets() {
        let selected = select_wayland_socket(
            vec![socket("wayland-0", 1), socket("wayland-1", 2)],
            &WaylandDisplay::new("wayland-1").unwrap(),
        )
        .unwrap();

        assert_eq!(selected.display().as_str(), "wayland-1");
        assert!(select_wayland_socket(
            vec![socket("wayland-0", 1)],
            &WaylandDisplay::new("wayland-2").unwrap(),
        )
        .unwrap_err()
        .contains("available: wayland-0"));
    }

    #[tokio::test]
    async fn disabled_wayland_skips_discovery_and_probe() {
        let port = Arc::new(CountingSessionPort::default());
        let sessions = SessionService::new(port.clone());
        let target = ShellTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        );
        let request = resolve_wayland_request(&sessions, &target, ShellWaylandSelection::Disabled)
            .await
            .unwrap();

        assert!(matches!(request, WaylandShellRequest::Disabled));
        assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 0);
        assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn automatic_wayland_uses_machine_aware_selection_only() {
        let port = Arc::new(CountingSessionPort::default());
        let sessions = SessionService::new(port.clone());
        let target = ShellTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        );

        let request = resolve_wayland_request(&sessions, &target, ShellWaylandSelection::Automatic)
            .await
            .unwrap();

        assert!(matches!(request, WaylandShellRequest::Disabled));
        assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
    }
}
