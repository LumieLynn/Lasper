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
/// * `LASPER_LOG_DIR` env var — set by the parent before re-exec; use as-is.
/// * Not root — normal user with intact XDG environment.
/// * Root without `LASPER_LOG_DIR` — true root session, system-global fallback.
fn get_log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LASPER_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if uzers::get_current_uid() != 0 {
        return dirs::state_dir()
            .map(|p| p.join("lasper"))
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .map(|p| p.join("lasper"))
                    .unwrap_or_else(|| PathBuf::from(".").join("lasper"))
            });
    }
    // True root session — no XDG env, no LASPER_LOG_DIR from a parent.
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

/// Re-execute the current binary via sudo. Returns `true` if the child exited
/// successfully (meaning the elevated instance took over), `false` otherwise.
fn try_elevate() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--elevate" && a != "-e")
        .collect();

    match std::process::Command::new("sudo")
        .arg("--preserve-env=PATH,TERM,COLORTERM,XDG_CONFIG_HOME,XDG_STATE_HOME,XDG_RUNTIME_DIR,HOME,USER,LOGNAME,LASPER_LOG_DIR,RUST_LOG,RUST_BACKTRACE")
        .arg(exe)
        .args(&args)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("lasper: sudo exited with status {}", s);
            false
        }
        Err(e) => {
            eprintln!("lasper: failed to run sudo: {}", e);
            false
        }
    }
}

fn print_help() {
    println!(
        "lasper {} — A TUI for managing systemd-nspawn containers.\n\n\
         USAGE:\n    lasper [FLAGS]\n\n\
         FLAGS:\n    -v, --version    Print version\n    -h, --help       Print this message\n    -e, --elevate    Request root elevation via sudo\n    -c, --cli-mode   Force CLI-only mode (skip DBus)\n\n\
         CONFIGURATION:\n    Settings are read from ~/.config/lasper/lasper.toml\n    [settings] elevate = true          Always request elevation.\n    [settings] cli-mode = true         Force CLI-only mode.\n    [settings] log-buffer-lines = N    Max log lines per container (default 5000).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Result of CLI flag parsing: either proceed with these options, or exit now.
fn parse_flags() -> std::result::Result<(bool, bool), i32> {
    let mut want_elevation = false;
    let mut want_cli_mode = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
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
            other => {
                eprintln!("lasper: unknown flag: {}", other);
                return Err(1);
            }
        }
    }
    Ok((want_elevation, want_cli_mode))
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI flags — all early exits happen here, before terminal
    //    takeover, so raw-mode / alternate-screen restoration is never needed.
    let (mut want_elevation, mut want_cli_mode) = match parse_flags() {
        Ok(opts) => opts,
        Err(code) => std::process::exit(code),
    };

    // 2. Load config for settings (elevation, cli_mode, log_buffer_lines)
    let app_settings = crate::config::load_settings();
    if let Some(ref settings) = app_settings {
        if !want_elevation {
            want_elevation = settings.elevate;
        }
        if !want_cli_mode {
            want_cli_mode = settings.cli_mode;
        }
    }

    // 3. Resolve log directory while XDG_STATE_HOME is still intact.
    //    Hand the pre-computed path to any re-exec'd child via env var so
    //    it doesn't need to re-detect env vars that sudo may have stripped.
    let log_dir = get_log_dir();
    std::env::set_var("LASPER_LOG_DIR", &log_dir);

    // 4. Elevate if requested and not already root
    if want_elevation && uzers::get_current_uid() != 0 {
        // Create the log directory now (user-owned) so the root child can
        // write into it without creating a root-owned parent directory.
        let _ = std::fs::create_dir_all(&log_dir);

        if try_elevate() {
            std::process::exit(0);
        }
        eprintln!("lasper: elevation failed, continuing in read-only mode");
    }

    // 5. Determine final privilege level
    let is_root = uzers::get_current_uid() == 0;

    // 6. Setup logging
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    cleanup_old_logs(&log_dir, 7);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "lasper.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    log::info!("Lasper starting (log dir: {})", log_dir.display());
    log::info!("Running as root: {}", is_root);

    // 6. Install panic hook
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));

    // 7. Initialize terminal
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize terminal")?;

    // 8. Run the application
    let log_buffer_lines = app_settings
        .as_ref()
        .map(|s| s.log_buffer_lines)
        .unwrap_or(0);
    let result = app::App::new(is_root, want_cli_mode, log_buffer_lines)
        .run(&mut terminal)
        .await;

    // 9. Restore terminal
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

    result
}
