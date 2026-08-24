//! Private per-session daemon logs, retention, and auth-log rate limiting.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const DAEMON_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const DAEMON_LOG_MAX_SESSIONS: usize = 8;
const DAEMON_LOG_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const DAEMON_AUTH_LOG_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct SessionLogWriter {
    state: Arc<parking_lot::Mutex<SessionLogState>>,
}

struct SessionLogState {
    file: std::fs::File,
    bytes_written: u64,
    max_bytes: u64,
    truncated: bool,
}

impl SessionLogWriter {
    fn new(file: std::fs::File) -> Self {
        Self::with_limit(file, DAEMON_LOG_MAX_BYTES)
    }

    pub(crate) fn with_limit(file: std::fs::File, max_bytes: u64) -> Self {
        Self {
            state: Arc::new(parking_lot::Mutex::new(SessionLogState {
                file,
                bytes_written: 0,
                max_bytes,
                truncated: false,
            })),
        }
    }

    fn write_truncation_marker(state: &mut SessionLogState) -> std::io::Result<()> {
        const MARKER: &[u8] = b"\n[daemon log truncated at the per-session limit]\n";
        let remaining = state.max_bytes.saturating_sub(state.bytes_written) as usize;
        if remaining > 0 {
            state
                .file
                .write_all(&MARKER[..remaining.min(MARKER.len())])?;
            state.bytes_written += remaining.min(MARKER.len()) as u64;
        }
        state.truncated = true;
        Ok(())
    }
}

impl Write for SessionLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self.state.lock();
        if state.truncated {
            return Ok(buf.len());
        }

        const MARKER_BYTES: u64 =
            b"\n[daemon log truncated at the per-session limit]\n".len() as u64;
        let content_limit = state
            .max_bytes
            .saturating_sub(MARKER_BYTES.min(state.max_bytes));
        let remaining = content_limit.saturating_sub(state.bytes_written) as usize;
        if buf.len() <= remaining {
            state.file.write_all(buf)?;
            state.bytes_written += buf.len() as u64;
            return Ok(buf.len());
        }

        if remaining > 0 {
            state.file.write_all(&buf[..remaining])?;
            state.bytes_written += remaining as u64;
        }
        Self::write_truncation_marker(&mut state)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.state.lock().file.flush()
    }
}

fn utc_log_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as libc::time_t)
        .unwrap_or_default();
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&seconds, &mut tm).is_null() {
            return seconds.to_string();
        }
        let mut buffer = [0u8; 32];
        let length = libc::strftime(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            c"%Y%m%dT%H%M%SZ".as_ptr(),
            &tm,
        );
        if length == 0 {
            seconds.to_string()
        } else {
            String::from_utf8_lossy(&buffer[..length as usize]).into_owned()
        }
    }
}

pub(crate) fn daemon_log_file_name() -> String {
    format!(
        "daemon-{}-p{}-s{}.log",
        utc_log_timestamp(),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

pub(crate) fn daemon_log_file_matches(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name
        .strip_prefix("daemon-")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    let Some((timestamp, process_and_session)) = stem.split_once("-p") else {
        return false;
    };
    let Some((pid, session)) = process_and_session.split_once("-s") else {
        return false;
    };
    let timestamp = timestamp.as_bytes();
    timestamp.len() == 16
        && timestamp[8] == b'T'
        && timestamp[15] == b'Z'
        && timestamp[..8].iter().all(u8::is_ascii_digit)
        && timestamp[9..15].iter().all(u8::is_ascii_digit)
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && session.len() == 32
        && session.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn cleanup_daemon_logs(
    directory: &Path,
    current: &Path,
    owner_uid: u32,
) -> std::io::Result<()> {
    use fs2::FileExt;
    use std::os::unix::fs::MetadataExt;

    struct Candidate {
        path: PathBuf,
        size: u64,
        modified: std::time::SystemTime,
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == current || !daemon_log_file_matches(&path) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.uid() != owner_uid || metadata.mode() & 0o077 != 0 {
            continue;
        }
        candidates.push(Candidate {
            path,
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        });
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));

    let mut kept = 1usize;
    let mut total_bytes = DAEMON_LOG_MAX_BYTES;
    for candidate in candidates {
        let retain = kept < DAEMON_LOG_MAX_SESSIONS
            && total_bytes.saturating_add(candidate.size) <= DAEMON_LOG_MAX_TOTAL_BYTES;
        if retain {
            kept += 1;
            total_bytes = total_bytes.saturating_add(candidate.size);
            continue;
        }

        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&candidate.path)
        {
            Ok(file) => file,
            Err(_) => continue,
        };
        if file.try_lock_exclusive().is_err() {
            continue;
        }
        let _ = std::fs::remove_file(&candidate.path);
        let _ = file.unlock();
    }
    Ok(())
}

fn open_daemon_session_log() -> std::io::Result<(PathBuf, SessionLogWriter)> {
    use fs2::FileExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let directory = crate::paths::log_dir();
    configure_daemon_log_directory(&directory, 0)?;

    let retention_lock_path = directory.join(".daemon-retention.lock");
    let retention_lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&retention_lock_path)?;
    std::fs::set_permissions(&retention_lock_path, std::fs::Permissions::from_mode(0o600))?;
    retention_lock.lock_exclusive()?;

    let path = directory.join(daemon_log_file_name());
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon log file ownership or mode verification failed",
        ));
    }

    file.lock_exclusive()?;
    cleanup_daemon_logs(&directory, &path, 0)?;
    retention_lock.unlock()?;
    Ok((path, SessionLogWriter::new(file)))
}

#[derive(Clone, Default)]
pub(crate) struct AuthLogLimiter {
    state: Arc<parking_lot::Mutex<AuthLogWindow>>,
}

#[derive(Default)]
struct AuthLogWindow {
    started: Option<std::time::Instant>,
    suppressed: u64,
}

impl AuthLogLimiter {
    pub(crate) fn record(&self, message: String) -> Option<String> {
        let now = std::time::Instant::now();
        let mut state = self.state.lock();
        if state
            .started
            .is_none_or(|started| now.duration_since(started) >= DAEMON_AUTH_LOG_WINDOW)
        {
            let suppressed = std::mem::take(&mut state.suppressed);
            state.started = Some(now);
            return Some(if suppressed == 0 {
                message
            } else {
                format!("{message} (suppressed {suppressed} similar events)")
            });
        }
        state.suppressed = state.suppressed.saturating_add(1);
        None
    }
}

pub(crate) fn configure_daemon_log_directory(
    path: &Path,
    expected_uid: u32,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "daemon log directory must be a real directory owned by uid {expected_uid}: {}",
                path.display()
            ),
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;

    let secured = std::fs::symlink_metadata(path)?;
    if !secured.is_dir() || secured.uid() != expected_uid || secured.mode() & 0o777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon log directory ownership or mode verification failed",
        ));
    }
    Ok(())
}

pub(crate) fn initialize_daemon_logging() -> std::io::Result<()> {
    let (path, writer) = open_daemon_session_log()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .try_init()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    log::info!("Daemon log session started: {}", path.display());
    Ok(())
}
