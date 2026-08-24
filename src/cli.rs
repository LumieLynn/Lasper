//! Process-level command-line parsing.
//!
//! This module only selects the process route.  It does not construct
//! application services or take ownership of terminal input, which leaves a
//! future non-interactive CLI free to reuse the same early dispatch boundary.

use std::path::PathBuf;

pub(crate) struct CliOptions {
    pub(crate) want_elevation: bool,
    pub(crate) want_cli_mode: bool,
    pub(crate) is_daemon: bool,
    pub(crate) fd_sock: Option<PathBuf>,
    pub(crate) rpc_sock: Option<PathBuf>,
    pub(crate) daemon_uid: u32,
    pub(crate) daemon_pid: u32,
}

fn print_help() {
    println!(
        "lasper {} — A TUI for managing systemd-nspawn containers.\n\n\
         USAGE:\n    lasper [FLAGS]\n\n\
         FLAGS:\n    -v, --version    Print version\n    -h, --help       Print this message\n    -e, --elevate    Use an isolated sudo daemon for privileged operations\n    -c, --cli-mode   Use runtime-state and systemd command backends\n\n\
         CONFIGURATION:\n    Settings are read from ~/.config/lasper/lasper.toml\n    [settings] elevate = true          Use the isolated sudo daemon.\n    [settings] cli-mode = true         Disable Lasper's direct DBus backend.\n    [settings] log-buffer-lines = N    Max log lines per container (default 5000).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Result of CLI flag parsing: either proceed with these options, or exit now.
pub(crate) fn parse_flags() -> std::result::Result<CliOptions, i32> {
    let mut options = CliOptions {
        want_elevation: false,
        want_cli_mode: false,
        is_daemon: false,
        fd_sock: None,
        rpc_sock: None,
        daemon_uid: 0,
        daemon_pid: 0,
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
            other => {
                eprintln!("lasper: unknown flag: {}", other);
                return Err(1);
            }
        }
        i += 1;
    }
    Ok(options)
}
