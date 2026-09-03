//! Process-level command-line parsing.
//!
//! This module only selects the process route.  It does not construct
//! application services or take ownership of terminal input, which leaves a
//! future non-interactive CLI free to reuse the same early dispatch boundary.

use std::path::PathBuf;

use crate::application::sessions::{ShellTarget, ValidatedGuestUserName};
use crate::domain::machine::MachineName;

pub(crate) struct CliOptions {
    pub(crate) want_elevation: bool,
    pub(crate) want_cli_mode: bool,
    pub(crate) is_daemon: bool,
    pub(crate) fd_sock: Option<PathBuf>,
    pub(crate) rpc_sock: Option<PathBuf>,
    pub(crate) daemon_uid: u32,
    pub(crate) daemon_pid: u32,
    shell: Option<ShellTarget>,
}

fn print_help() {
    println!(
        "lasper {} — A TUI for managing systemd-nspawn containers.\n\n\
         USAGE:\n    lasper [FLAGS]\n    lasper shell USER@MACHINE\n\n\
         FLAGS:\n    -v, --version    Print version\n    -h, --help       Print this message\n    -e, --elevate    Use an isolated sudo daemon for privileged operations\n    -c, --cli-mode   Use runtime-state and systemd command backends\n\n\
         CONFIGURATION:\n    Settings are read from ~/.config/lasper/lasper.toml\n    [settings] elevate = true          Use the isolated sudo daemon.\n    [settings] cli-mode = true         Disable Lasper's direct DBus backend.\n    [settings] log-buffer-lines = N    Max log lines per container (default 5000).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Dispatch process-level commands before the TUI owns the terminal.
pub(crate) fn dispatch() -> std::result::Result<CliOptions, i32> {
    let mut options = parse_flags()?;
    if let Some(target) = options.shell.take() {
        return Err(run_shell(target));
    }
    Ok(options)
}

fn run_shell(target: ShellTarget) -> i32 {
    let machinectl_target = format!("{}@{}", target.user(), target.machine());
    match std::process::Command::new("machinectl")
        .args(["--", "shell", machinectl_target.as_str()])
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("lasper: failed to start machinectl shell: {error}");
            1
        }
    }
}

fn parse_shell_target(target: &str) -> Result<ShellTarget, String> {
    let Some((user, machine)) = target.split_once('@') else {
        return Err("shell target must be USER@MACHINE".into());
    };
    if machine.is_empty() || machine.contains('@') || user.contains('@') {
        return Err("shell target must be USER@MACHINE".into());
    }
    let machine = MachineName::new(machine)
        .map_err(|error| format!("invalid machine in shell target: {error}"))?;
    let user = ValidatedGuestUserName::new(user)
        .map_err(|error| format!("invalid user in shell target: {error}"))?;
    Ok(ShellTarget::new(machine, user))
}

fn parse_flags() -> std::result::Result<CliOptions, i32> {
    let mut options = CliOptions {
        want_elevation: false,
        want_cli_mode: false,
        is_daemon: false,
        fd_sock: None,
        rpc_sock: None,
        daemon_uid: 0,
        daemon_pid: 0,
        shell: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            "shell" => {
                if options.shell.is_some() {
                    eprintln!("lasper: shell target may only be specified once");
                    return Err(1);
                }
                i += 1;
                let Some(target) = args.get(i) else {
                    eprintln!("lasper: shell requires USER@MACHINE");
                    return Err(1);
                };
                options.shell = Some(parse_shell_target(target).map_err(|error| {
                    eprintln!("lasper: {error}");
                    1
                })?);
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
}
