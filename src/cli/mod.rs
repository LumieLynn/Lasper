//! Process entry points and parsed invocation data.
//!
//! Parsing and help finish before a runtime is built. Session execution delegates
//! to application services; concrete service composition remains in main.

mod help;
mod parse;
mod session;

use std::path::PathBuf;

use crate::adapters::elevated::ElevationMethod;
use crate::application::sessions::{GuestCommand, ShellTarget, WaylandShellRequest};

pub(crate) use session::{report_command_error, run_launch, run_shell};

pub(crate) const BUILD_VERSION: &str = match option_env!("LASPER_BUILD_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CliOptions {
    pub(crate) want_elevation: bool,
    pub(crate) elevation_method: Option<ElevationMethod>,
    pub(crate) want_systemd_tools: bool,
}

#[derive(Debug, Default)]
pub(crate) struct DaemonOptions {
    pub(crate) fd_sock: Option<PathBuf>,
    pub(crate) rpc_sock: Option<PathBuf>,
    pub(crate) daemon_uid: u32,
    pub(crate) daemon_pid: u32,
}

#[derive(Debug)]
pub(crate) enum CliDispatch {
    Application(CliOptions),
    Shell(Box<ShellCommand>),
    Daemon(DaemonOptions),
    X11Reconcile { want_systemd_tools: bool },
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
    wayland: WaylandShellRequest,
    x11: ShellX11Selection,
    command: Option<GuestCommand>,
    options: CliOptions,
    quiet: bool,
}

impl ShellCommand {
    pub(crate) fn target(&self) -> &ShellTarget {
        &self.target
    }

    pub(crate) const fn io_mode(&self) -> ShellIoMode {
        self.io_mode
    }

    pub(crate) const fn permits_elevation(&self) -> bool {
        matches!(self.io_mode, ShellIoMode::Interactive)
    }

    pub(crate) const fn wants_elevation(&self) -> bool {
        self.options.want_elevation
    }

    pub(crate) const fn elevation_method(&self) -> Option<ElevationMethod> {
        self.options.elevation_method
    }

    pub(crate) const fn wants_systemd_tools(&self) -> bool {
        self.options.want_systemd_tools
    }

    pub(crate) fn command(&self) -> Option<&GuestCommand> {
        self.command.as_ref()
    }

    pub(crate) const fn allows_wayland_fallback(&self) -> bool {
        matches!(self.io_mode, ShellIoMode::Interactive)
            && matches!(self.wayland, WaylandShellRequest::Automatic)
    }

    pub(crate) const fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub(crate) const fn requests_x11(&self) -> bool {
        !matches!(self.x11, ShellX11Selection::Disabled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellX11Selection {
    Disabled,
    Current,
    Display(u16),
}

pub(crate) fn dispatch() -> Result<CliDispatch, i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse::parse(&args) {
        Ok(parse::Invocation::Run(dispatch)) => Ok(dispatch),
        Ok(parse::Invocation::Help(topic)) => {
            print!("{}", help::text(topic));
            Err(0)
        }
        Ok(parse::Invocation::Version) => {
            println!("lasper {BUILD_VERSION}");
            Err(0)
        }
        Err(error) => {
            eprintln!("lasper: {error}");
            Err(1)
        }
    }
}
