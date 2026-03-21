use std::io;
use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

mod app;
mod events;
mod nspawn;
mod ui;

use std::env;
use std::ffi::{CStr, CString};
use std::os::unix::fs::chown;
use std::path::{Path, PathBuf};

fn get_user_home(username: &str) -> Option<PathBuf> {
    let username_c = CString::new(username).ok()?;
    unsafe {
        let pw = libc::getpwnam(username_c.as_ptr());
        if !pw.is_null() {
            let home = CStr::from_ptr((*pw).pw_dir);
            return Some(PathBuf::from(home.to_string_lossy().into_owned()));
        }
    }
    None
}

fn get_log_dir() -> PathBuf {
    if let Ok(sudo_user) = env::var("SUDO_USER") {
        if sudo_user != "root" {
            if let Some(home) = get_user_home(&sudo_user) {
                return home.join(".local/share/lasper");
            }
        }
    }
    dirs::data_local_dir()
        .map(|p| p.join("lasper"))
        .unwrap_or_else(|| PathBuf::from(".").join("lasper"))
}

fn try_chown_to_sudo_user(path: &Path) {
    let sudo_uid = env::var("SUDO_UID").ok().and_then(|s| s.parse::<u32>().ok());
    let sudo_gid = env::var("SUDO_GID").ok().and_then(|s| s.parse::<u32>().ok());

    if let (Some(uid), Some(gid)) = (sudo_uid, sudo_gid) {
        if let Err(e) = chown(path, Some(uid), Some(gid)) {
            eprintln!("Warning: Failed to chown {:?} to {}:{}: {}", path, uid, gid, e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup file-based logging to $HOME/.local/share/lasper/app.log
    let log_dir = get_log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    try_chown_to_sudo_user(&log_dir);

    let file_appender = tracing_appender::rolling::never(&log_dir, "app.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    try_chown_to_sudo_user(&log_dir.join("app.log"));

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    log::info!("Lasper starting");

    // Detect privilege level (uid 0 = root)
    let is_root = unsafe { libc::getuid() } == 0;
    log::info!("Running as root: {}", is_root);

    // Install panic hook to restore terminal before printing panic info
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));

    // Initialize terminal
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize terminal")?;

    // Run the application
    let result = app::App::new(is_root).run(&mut terminal).await;

    // Always restore terminal
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
