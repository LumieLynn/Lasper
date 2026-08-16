use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;

mod app;
mod config;
mod events;
mod nspawn;
mod paths;
mod term;
mod ui;

use std::path::{Path, PathBuf};

struct TerminalRestoreGuard<F: FnMut()> {
    restore: F,
    armed: bool,
}

impl<F: FnMut()> TerminalRestoreGuard<F> {
    fn new(restore: F) -> Self {
        Self {
            restore,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn restore(&mut self) {
        if std::mem::take(&mut self.armed) {
            (self.restore)();
        }
    }
}

impl<F: FnMut()> Drop for TerminalRestoreGuard<F> {
    fn drop(&mut self) {
        self.restore();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

/// Resolve the log directory.
///
/// * Not root — user's XDG state directory.
/// * Root — system-global fallback.
fn get_log_dir() -> PathBuf {
    if uzers::get_current_uid() != 0 {
        return dirs::state_dir()
            .map(|p| p.join("lasper"))
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .map(|p| p.join("lasper"))
                    .unwrap_or_else(|| PathBuf::from(".").join("lasper"))
            });
    }
    crate::paths::log_dir()
}

fn cleanup_old_logs(log_dir: &Path, keep: usize) {
    if let Ok(entries) = std::fs::read_dir(log_dir) {
        let mut logs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file() && e.file_name().to_string_lossy().starts_with("lasper"))
            .collect();

        // Sort by modification time, newest first
        logs.sort_by_key(|e| {
            std::cmp::Reverse(
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
        });

        // Delete older logs
        for log in logs.into_iter().skip(keep) {
            let _ = std::fs::remove_file(log.path());
        }
    }
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

struct CliOptions {
    want_elevation: bool,
    want_cli_mode: bool,
    is_daemon: bool,
    fd_sock: Option<PathBuf>,
    rpc_sock: Option<PathBuf>,
    daemon_uid: u32,
    daemon_pid: u32,
}

/// Result of CLI flag parsing: either proceed with these options, or exit now.
fn parse_flags() -> std::result::Result<CliOptions, i32> {
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

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI flags — all early exits happen here, before terminal
    //    takeover, so raw-mode / alternate-screen restoration is never needed.
    let options = match parse_flags() {
        Ok(options) => options,
        Err(code) => std::process::exit(code),
    };
    let want_elevation = options.want_elevation;
    let mut want_cli_mode = options.want_cli_mode;

    // 1b. Internal daemon mode — run as root child process, exit early.
    if options.is_daemon {
        crate::nspawn::sys::daemon::daemon_main(
            options.fd_sock,
            options.rpc_sock,
            options.daemon_uid,
            options.daemon_pid,
        )
        .await;
    }

    // 2. Parse configuration once for settings, theme, and bootstrap profiles.
    let loaded_config = crate::config::load_config();
    let config_diagnostic = loaded_config.diagnostic;
    let app_config = std::sync::Arc::new(loaded_config.config);
    let app_settings = &app_config.settings;
    if !want_cli_mode {
        want_cli_mode = app_settings.cli_mode;
    }

    // 3. Permission manager — no full-process elevation.
    //    `-e` / `elevate = true` routes privileged work through a sudo daemon.
    let use_sudo =
        crate::nspawn::ops::DefaultPermissionManager::wants_elevation(want_elevation, app_settings);
    let pm: std::sync::Arc<dyn crate::nspawn::ops::PermissionManager> = std::sync::Arc::new(
        crate::nspawn::ops::DefaultPermissionManager::new().with_elevation(use_sudo),
    );

    // 3b. Spawn elevated daemon before terminal takeover so the sudo
    //     password prompt (if any) appears on the clean terminal.
    let daemon: Option<std::sync::Arc<crate::nspawn::sys::daemon::ElevatedDaemon>> =
        if pm.level() == crate::nspawn::ops::PermissionLevel::Elevated {
            match crate::nspawn::sys::daemon::ElevatedDaemon::spawn(!want_cli_mode).await {
                Ok(d) => Some(std::sync::Arc::new(d)),
                Err(e) => {
                    eprintln!("Failed to start elevated daemon: {}", e);
                    eprintln!(
                        "For sudo errors, run `sudo -v`; otherwise inspect the reported \
                         path or permission details."
                    );
                    std::process::exit(1);
                }
            }
        } else {
            None
        };

    // 3c. Build execution context — one-time routing for commands and file I/O.
    let exec_ctx = std::sync::Arc::new(crate::nspawn::sys::ExecutionContext::new(
        pm.level(),
        daemon,
    )?);

    // 4. Setup logging — always owned by the current user.
    let log_dir = get_log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    cleanup_old_logs(&log_dir, 7);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "lasper.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    log::info!("Lasper starting (log dir: {})", log_dir.display());
    log::info!("Elevation mode: {}", use_sudo);

    // 5. Install panic hook
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));

    // 6. Initialize terminal
    let mut terminal_restore = TerminalRestoreGuard::new(restore_terminal);
    enable_raw_mode().context("Failed to enable raw mode")?;
    terminal_restore.arm();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize terminal")?;

    // 7. Run the application
    let log_buffer_lines = app_settings.log_buffer_lines;
    let mut app = app::App::new(
        pm,
        want_cli_mode,
        log_buffer_lines,
        exec_ctx.clone(),
        app_config,
    );
    if let Some(diagnostic) = config_diagnostic {
        log::warn!("{}", diagnostic.detail);
        app.set_status_for(
            diagnostic.summary,
            crate::ui::StatusLevel::Warn,
            std::time::Duration::from_secs(12),
        );
    }
    let result = app.run(&mut terminal).await;

    log::info!("[lasper] run() completed, restoring terminal...");

    // 8. Restore terminal
    terminal_restore.restore();
    let _ = terminal.show_cursor();

    if let Err(ref e) = result {
        log::error!("Application error: {:#}", e);
        eprintln!("Error: {:#}", e);
    }

    log::info!("[lasper] calling daemon.exit()...");
    exec_ctx.exit_daemon().await;
    log::info!("[lasper] daemon.exit() completed");

    log::info!("[lasper] main() returning");
    let code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(code);
}

#[cfg(test)]
mod terminal_restore_tests {
    use super::TerminalRestoreGuard;
    use std::cell::Cell;

    #[test]
    fn armed_guard_restores_on_early_return_and_only_once() {
        let calls = Cell::new(0);
        {
            let mut guard = TerminalRestoreGuard::new(|| calls.set(calls.get() + 1));
            guard.arm();
        }
        assert_eq!(calls.get(), 1);

        {
            let mut guard = TerminalRestoreGuard::new(|| calls.set(calls.get() + 1));
            guard.arm();
            guard.restore();
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unarmed_guard_does_not_restore() {
        let calls = Cell::new(0);
        {
            let _guard = TerminalRestoreGuard::new(|| calls.set(calls.get() + 1));
        }
        assert_eq!(calls.get(), 0);
    }
}
