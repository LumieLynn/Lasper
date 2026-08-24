//! Application-process logging setup.

use anyhow::{Context, Result};
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

        // Sort by modification time, newest first.
        logs.sort_by_key(|e| {
            std::cmp::Reverse(
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
        });

        // Delete older logs.
        for log in logs.into_iter().skip(keep) {
            let _ = std::fs::remove_file(log.path());
        }
    }
}

/// Initialize the application logger and return its directory plus worker
/// guard. The guard must stay alive for the whole process lifetime.
pub(crate) fn init() -> Result<(PathBuf, tracing_appender::non_blocking::WorkerGuard)> {
    let log_dir = get_log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    cleanup_old_logs(&log_dir, 7);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "lasper.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    Ok((log_dir, guard))
}
