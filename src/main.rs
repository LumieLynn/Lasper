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
         FLAGS:\n    -v, --version    Print version\n    -h, --help       Print this message\n    -e, --elevate    Use sudo for privileged CLI commands\n    -c, --cli-mode   Force CLI-only mode (skip DBus)\n\n\
         CONFIGURATION:\n    Settings are read from ~/.config/lasper/lasper.toml\n    [settings] elevate = true          Use sudo for privileged CLI commands.\n    [settings] cli-mode = true         Force CLI-only mode.\n    [settings] log-buffer-lines = N    Max log lines per container (default 5000).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Result of CLI flag parsing: either proceed with these options, or exit now.
fn parse_flags() -> std::result::Result<(bool, bool, bool, Option<String>, u32), i32> {
    let mut want_elevation = false;
    let mut want_cli_mode = false;
    let mut is_daemon = false;
    let mut fd_sock: Option<String> = None;
    let mut daemon_uid: u32 = 0;
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
            "--elevate" | "-e" => want_elevation = true,
            "--cli-mode" | "-c" => want_cli_mode = true,
            "--daemon" => is_daemon = true,
            "--fd-sock" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --fd-sock requires a path argument");
                    return Err(1);
                }
                fd_sock = Some(args[i].clone());
            }
            "--daemon-uid" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("lasper: --daemon-uid requires a uid argument");
                    return Err(1);
                }
                daemon_uid = match args[i].parse::<u32>() {
                    Ok(uid) => uid,
                    Err(_) => {
                        eprintln!("lasper: --daemon-uid must be a positive integer");
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
    Ok((want_elevation, want_cli_mode, is_daemon, fd_sock, daemon_uid))
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI flags — all early exits happen here, before terminal
    //    takeover, so raw-mode / alternate-screen restoration is never needed.
    let (want_elevation, mut want_cli_mode, is_daemon, fd_sock, daemon_uid) = match parse_flags() {
        Ok(opts) => opts,
        Err(code) => std::process::exit(code),
    };

    // 1b. Internal daemon mode — run as root child process, exit early.
    if is_daemon {
        crate::nspawn::sys::daemon::daemon_main(fd_sock, daemon_uid).await;
    }

    // 2. Load config for settings (elevation, cli_mode, log_buffer_lines)
    let app_settings = crate::config::load_settings();
    if let Some(ref settings) = app_settings {
        if !want_cli_mode {
            want_cli_mode = settings.cli_mode;
        }
    }

    // 3. Permission manager — no full-process elevation.
    //    `-e` / `elevate = true` means sudo wraps privileged CLI commands.
    let use_sudo = crate::nspawn::ops::DefaultPermissionManager::wants_elevation(
        want_elevation,
        &app_settings,
    );
    let pm: std::sync::Arc<dyn crate::nspawn::ops::PermissionManager> = std::sync::Arc::new(
        crate::nspawn::ops::DefaultPermissionManager::new().with_elevation(use_sudo),
    );

    // 3b. Spawn elevated daemon before terminal takeover so the sudo
    //     password prompt (if any) appears on the clean terminal.
    let daemon: Option<std::sync::Arc<crate::nspawn::sys::daemon::ElevatedDaemon>> =
        if pm.level() == crate::nspawn::ops::PermissionLevel::Elevated {
            match crate::nspawn::sys::daemon::ElevatedDaemon::spawn().await {
                Ok(d) => Some(std::sync::Arc::new(d)),
                Err(e) => {
                    eprintln!("Failed to spawn elevated daemon: {}", e);
                    eprintln!("Make sure you have sudo privileges.");
                    std::process::exit(1);
                }
            }
        } else {
            None
        };

    // 3c. Build execution context — one-time routing for commands and file I/O.
    let exec_ctx = std::sync::Arc::new(
        crate::nspawn::sys::ExecutionContext::new(pm.level(), daemon),
    );

    // 4. Setup logging — always owned by the current user.
    let log_dir = get_log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    cleanup_old_logs(&log_dir, 7);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "lasper.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
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
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize terminal")?;

    // 7. Run the application
    let log_buffer_lines = app_settings
        .as_ref()
        .map(|s| s.log_buffer_lines)
        .unwrap_or(0);
    let result = app::App::new(pm, want_cli_mode, log_buffer_lines, exec_ctx.clone())
        .run(&mut terminal)
        .await;

    log::info!("[lasper] run() completed, restoring terminal...");

    // 8. Restore terminal
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
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
