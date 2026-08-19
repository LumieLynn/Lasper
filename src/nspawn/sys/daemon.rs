//! Elevated daemon — a long-running root child process that executes closed,
//! typed privileged operations on behalf of the unprivileged TUI.
//!
//! The daemon serves JSON-RPC 2.0 over a mutually authenticated Unix stream
//! (one JSON object per line). The daemon is spawned via
//! `sudo <self> --daemon` before the terminal enters raw mode, so the sudo
//! password prompt appears on the clean terminal.
//!
//! ## Architecture
//!
//! A dedicated I/O task serializes all RPC traffic through the authenticated
//! Unix stream. Callers send `(request, oneshot::Sender)` tuples over an mpsc
//! channel; the I/O task writes the request, reads the response, and fulfills
//! the oneshot. This avoids locking issues around async socket I/O.
//!
//! ## FD passing
//!
//! Long-running commands (journalctl -f, terminal attachment) are spawned by
//! the daemon as root. The daemon passes the resulting fd back to the parent
//! over a Unix domain socket using the [`sendfd`] crate. The socket is scoped
//! to a private per-session directory, owned by the launching user, and each
//! connection must match both the TUI's PID/UID via `SO_PEERCRED` and a random
//! session token negotiated after the control connection's peer credentials
//! have been authenticated.

use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::adapters::config::store::{
    execute_nspawn_config_operation, NspawnConfigOperation, NspawnConfigResult,
};
use crate::nspawn::adapters::config::systemd_unit::{
    execute_systemd_unit_operation, SystemdUnitOperation, SystemdUnitResult,
};
use crate::nspawn::adapters::rootfs::store::{
    execute_rootfs_operation, RootfsOperation, RootfsResult,
};
use crate::nspawn::adapters::storage::store::{
    execute_managed_storage_operation, ManagedStorageOperation, ManagedStorageResult,
};
use crate::nspawn::models::{
    ContainerEntry, ImageEntry, MachineName, MachineProperties, TerminalSize,
};
use crate::nspawn::ops::provision::bootstrap_operation::{
    build_command as build_bootstrap_command, probe_debootstrap_signature_style_sync,
    validate_target as validate_bootstrap_target, BootstrapRequest,
};
use crate::nspawn::ops::provision::image_operation::{
    inspect_tar_runtime, ImageImportReport, ImportTarRequest, TarRuntimeAssessment,
};
use crate::nspawn::ops::provision::oci_operation::{
    run_oci_transfer, OciPullRequest, OciTransferCancellation, OciTransferOutcome,
};
use crate::nspawn::ops::system_operation::{
    execute_dbus_system_operation, execute_system_operation, SystemOperation,
};
use crate::nspawn::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation, NvidiaStateResult,
};
use crate::nspawn::sys::terminal_attach::TerminalAttachKind;
use sendfd::{RecvWithFd, SendWithFd};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

// ── RPC message types ──

const RPC_PROTOCOL_VERSION: u32 = 9;
const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;
const MAX_FD_CONNECTIONS: usize = 32;
const DAEMON_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DAEMON_LOG_MAX_SESSIONS: usize = 8;
const DAEMON_LOG_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const DAEMON_AUTH_LOG_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
const DAEMON_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcBootstrap {
    protocol_version: u32,
    auth_token: String,
    dbus_enabled: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliInspectMachineRequest {
    machine: MachineName,
}

#[derive(Serialize, Deserialize)]
struct FdRequest {
    auth_token: String,
    #[serde(flatten)]
    operation: FdOperation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
enum FdOperation {
    #[serde(rename = "spawn_journalctl")]
    Journalctl(SpawnJournalctlParams),
    #[serde(rename = "spawn_terminal")]
    Terminal(SpawnTerminalParams),
    #[serde(rename = "spawn_bootstrap")]
    Bootstrap(Box<SpawnBootstrapParams>),
    #[serde(rename = "spawn_oci_pull")]
    OciPull(Box<SpawnOciPullParams>),
    #[serde(rename = "import_raw_image")]
    ImportRawImage(ImportRawImageParams),
    #[serde(rename = "import_tar_image")]
    ImportTarImage(ImportTarRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnJournalctlParams {
    name: MachineName,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnTerminalParams {
    name: MachineName,
    size: TerminalSize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnTerminalResponse {
    attach_kind: TerminalAttachKind,
}

pub(crate) struct SpawnedTerminalPty {
    pub master_fd: RawFd,
    pub attach_kind: TerminalAttachKind,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnBootstrapParams {
    cmd_id: u64,
    request: BootstrapRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnOciPullParams {
    cmd_id: u64,
    request: OciPullRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRawImageParams {
    machine: MachineName,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportImageResponse {
    #[serde(default)]
    warnings: Vec<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct RpcResponse {
    jsonrpc: String,
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

pub(crate) fn pipe_reader(fd: RawFd) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::OwnedFd;
    use std::os::unix::io::FromRawFd;
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    tokio::net::unix::pipe::Receiver::from_owned_fd(owned)
}

type RpcCall = (RpcRequest, tokio::sync::oneshot::Sender<RpcResponse>);

enum HandleOutcome {
    Spawned,
    Sync(Result<serde_json::Value, String>),
}

async fn initialize_dbus_backend(
    enabled: bool,
) -> Option<crate::nspawn::adapters::comm::dbus::DbusBackend> {
    if !enabled {
        return None;
    }

    let dbus = crate::nspawn::adapters::comm::dbus::DbusBackend::new();
    ContainerBackend::is_available(&dbus).await.then_some(dbus)
}

/// The D-Bus surface used by the RPC dispatcher.
///
/// This is intentionally private to the daemon for now. It keeps dispatch
/// tests independent from a live system bus without pretending that the
/// application already has a general host capability layer.
#[async_trait::async_trait]
trait DaemonDbusExecutor: Send + Sync {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>>;
    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>>;
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()>;
    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties>;
    async fn is_available(&self) -> bool;
}

#[async_trait::async_trait]
impl DaemonDbusExecutor for crate::nspawn::adapters::comm::dbus::DbusBackend {
    async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>> {
        ContainerBackend::list_machines(self).await
    }

    async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>> {
        ContainerBackend::list_images(self).await
    }

    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::nspawn::errors::Result<()> {
        execute_dbus_system_operation(self, operation).await
    }

    async fn get_properties(&self, name: &str) -> crate::nspawn::errors::Result<MachineProperties> {
        ContainerBackend::get_properties(self, name).await
    }

    async fn is_available(&self) -> bool {
        ContainerBackend::is_available(self).await
    }
}

// ── Parent-side handle ──

pub struct ElevatedDaemon {
    request_tx: tokio::sync::mpsc::Sender<RpcCall>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    event_tx: tokio::sync::broadcast::Sender<()>,
    pid: u32,
    rpc_sock_path: PathBuf,
    fd_sock_path: PathBuf,
    fd_auth_token: Arc<str>,
    _fd_sock_dir: Arc<tempfile::TempDir>,
    next_spawn_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    spawn_exit_codes: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<u64, i32>>>,
}

impl std::fmt::Debug for ElevatedDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElevatedDaemon")
            .field("pid", &self.pid)
            .field("fd_sock_path", &self.fd_sock_path)
            .field("fd_auth_token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Clone for ElevatedDaemon {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
            next_id: self.next_id.clone(),
            event_tx: self.event_tx.clone(),
            pid: self.pid,
            rpc_sock_path: self.rpc_sock_path.clone(),
            fd_sock_path: self.fd_sock_path.clone(),
            fd_auth_token: self.fd_auth_token.clone(),
            _fd_sock_dir: self._fd_sock_dir.clone(),
            next_spawn_id: self.next_spawn_id.clone(),
            spawn_exit_codes: self.spawn_exit_codes.clone(),
        }
    }
}

impl PartialEq for ElevatedDaemon {
    fn eq(&self, other: &Self) -> bool {
        self.pid == other.pid
    }
}

fn create_fd_socket_dir(user_uid: u32) -> std::io::Result<tempfile::TempDir> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if is_private_writable_runtime_dir(&path, user_uid) {
            candidates.push(path);
        }
    }
    let system_runtime = PathBuf::from(format!("/run/user/{user_uid}"));
    if !candidates.contains(&system_runtime)
        && is_private_writable_runtime_dir(&system_runtime, user_uid)
    {
        candidates.push(system_runtime);
    }

    create_fd_socket_dir_from_candidates(user_uid, &candidates)
}

fn create_fd_socket_dir_from_candidates(
    user_uid: u32,
    candidates: &[PathBuf],
) -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::MetadataExt;

    let mut directory = None;
    for path in candidates {
        match create_private_tempdir(Some(path)) {
            Ok(candidate) => {
                directory = Some(candidate);
                break;
            }
            Err(error) if runtime_dir_error_allows_fallback(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    let directory = match directory {
        Some(directory) => directory,
        None => create_private_tempdir(None)?,
    };

    let metadata = std::fs::symlink_metadata(directory.path())?;
    if metadata.uid() != user_uid || metadata.mode() & 0o777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "fd socket directory ownership or mode verification failed: \
                 path={}, uid={} (expected {}), mode={:o} (expected 700)",
                directory.path().display(),
                metadata.uid(),
                user_uid,
                metadata.mode() & 0o777
            ),
        ));
    }
    Ok(directory)
}

fn create_private_tempdir(parent: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;

    let mut builder = tempfile::Builder::new();
    builder.prefix("lasper-");
    builder.permissions(std::fs::Permissions::from_mode(0o700));
    match parent {
        Some(path) => builder.tempdir_in(path),
        None => builder.tempdir(),
    }
}

fn is_private_writable_runtime_dir(path: &Path, user_uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == user_uid
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o300 == 0o300
    })
}

fn runtime_dir_error_allows_fallback(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotFound
    )
}

fn configure_user_socket(path: &Path, user_uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.uid() != user_uid {
        std::os::unix::fs::chown(path, Some(user_uid), None)?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    let secured = std::fs::symlink_metadata(path)?;
    if secured.uid() != user_uid || secured.mode() & 0o777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon socket ownership or mode verification failed",
        ));
    }
    Ok(())
}

async fn connect_rpc_socket(path: &Path) -> std::io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Read one protocol frame without allowing a peer to grow an unbounded line
/// in memory. The connection is discarded by callers when this returns an
/// error, so consuming an oversized prefix is intentional.
async fn read_bounded_line<R>(reader: &mut R, limit: usize) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut limited = reader.take((limit as u64).saturating_add(1));
    let mut bytes = Vec::new();
    let count = limited.read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("protocol frame exceeds {limit} bytes"),
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[derive(Clone)]
struct SessionLogWriter {
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

    fn with_limit(file: std::fs::File, max_bytes: u64) -> Self {
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

fn daemon_log_file_name() -> String {
    format!(
        "daemon-{}-p{}-s{}.log",
        utc_log_timestamp(),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

fn daemon_log_file_matches(path: &Path) -> bool {
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

fn cleanup_daemon_logs(directory: &Path, current: &Path, owner_uid: u32) -> std::io::Result<()> {
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
struct AuthLogLimiter {
    state: Arc<parking_lot::Mutex<AuthLogWindow>>,
}

#[derive(Default)]
struct AuthLogWindow {
    started: Option<std::time::Instant>,
    suppressed: u64,
}

impl AuthLogLimiter {
    fn record(&self, message: String) -> Option<String> {
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

fn require_daemon_root(effective_uid: u32) -> std::io::Result<()> {
    if effective_uid == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "lasper daemon must run as root",
        ))
    }
}

fn configure_daemon_log_directory(path: &Path, expected_uid: u32) -> std::io::Result<()> {
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

fn initialize_daemon_logging() -> std::io::Result<()> {
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

fn open_pidfd(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "parent PID is out of range",
        )
    })?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let flags = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pidfd)
}

fn monitor_parent(pidfd: OwnedFd) -> std::io::Result<()> {
    let pidfd = tokio::io::unix::AsyncFd::new(pidfd)?;
    tokio::spawn(async move {
        match pidfd.readable().await {
            Ok(_) => log::info!("Daemon: launching TUI exited; stopping elevated daemon"),
            Err(error) => log::error!("Daemon: parent pidfd monitor failed: {error}"),
        }
        shutdown_daemon_resources().await;
        std::process::exit(0);
    });
    Ok(())
}

impl ElevatedDaemon {
    pub async fn spawn(dbus_enabled: bool) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let user_uid = uzers::get_current_uid();
        let parent_pid = std::process::id();
        let fd_auth_token: Arc<str> = Arc::from(uuid::Uuid::new_v4().to_string());
        let fd_sock_dir = Arc::new(create_fd_socket_dir(user_uid)?);
        let fd_sock_path = fd_sock_dir.path().join("fd.sock");
        let rpc_sock_path = fd_sock_dir.path().join("rpc.sock");

        let mut child = tokio::process::Command::new("sudo")
            .kill_on_drop(true)
            .arg(&exe)
            .arg("--daemon")
            .arg("--fd-sock")
            .arg(&fd_sock_path)
            .arg("--rpc-sock")
            .arg(&rpc_sock_path)
            .arg("--daemon-uid")
            .arg(user_uid.to_string())
            .arg("--daemon-pid")
            .arg(parent_pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("failed to launch sudo daemon: {}", error),
                )
            })?;

        let pid = child.id().expect("child has pid");

        use tokio::io::AsyncWriteExt;
        let mut rpc_stream = connect_rpc_socket(&rpc_sock_path).await?;
        authorize_root_server(get_peer_credentials(&rpc_stream)?)?;
        let rpc_bootstrap = RpcBootstrap {
            protocol_version: RPC_PROTOCOL_VERSION,
            auth_token: fd_auth_token.to_string(),
            dbus_enabled,
        };
        let mut rpc_bootstrap_line = serde_json::to_vec(&rpc_bootstrap).map_err(|error| {
            std::io::Error::other(format!("serialize RPC authentication: {error}"))
        })?;
        rpc_bootstrap_line.push(b'\n');
        rpc_stream.write_all(&rpc_bootstrap_line).await?;
        rpc_stream.flush().await?;
        let (rpc_reader, rpc_writer) = rpc_stream.into_split();

        let (request_tx, mut request_rx) = tokio::sync::mpsc::channel::<RpcCall>(8);
        let (event_tx, _) = tokio::sync::broadcast::channel::<()>(16);

        let io_pid = pid;
        let event_tx_io = event_tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;

            let mut writer = tokio::io::BufWriter::new(rpc_writer);
            let mut reader = tokio::io::BufReader::new(rpc_reader);

            let mut pending: std::collections::HashMap<
                u64,
                tokio::sync::oneshot::Sender<RpcResponse>,
            > = std::collections::HashMap::new();

            loop {
                tokio::select! {
                    req_opt = request_rx.recv() => {
                        let (request, response_tx) = match req_opt {
                            Some(r) => r,
                            None => break,
                        };

                        let id = request.id;

                        let mut req_line = match serde_json::to_string(&request) {
                            Ok(s) => s,
                            Err(e) => {
                                log::error!("Daemon I/O: failed to serialize request: {}", e);
                                continue;
                            }
                        };
                        req_line.push('\n');
                        if req_line.len() > MAX_RPC_FRAME_BYTES {
                            let _ = response_tx.send(RpcResponse {
                                jsonrpc: "2.0".into(),
                                id,
                                result: None,
                                error: Some(RpcError {
                                    code: -1,
                                    message: format!(
                                        "daemon request exceeds {MAX_RPC_FRAME_BYTES} bytes"
                                    ),
                                }),
                            });
                            continue;
                        }

                        if let Err(e) = writer.write_all(req_line.as_bytes()).await {
                            log::error!("Daemon I/O: failed to write to daemon RPC socket: {}", e);
                            break;
                        }
                        if let Err(e) = writer.flush().await {
                            log::error!("Daemon I/O: failed to flush daemon RPC socket: {}", e);
                            break;
                        }

                        pending.insert(id, response_tx);
                    }

                    read_res = read_bounded_line(&mut reader, MAX_RPC_FRAME_BYTES) => {
                        match read_res {
                            Ok(None) => {
                                log::info!("[lasper] I/O task: daemon RPC socket EOF");
                                log::error!("Daemon I/O: RPC socket closed (EOF)");
                                break;
                            }
                            Ok(Some(line)) => {
                                let raw: serde_json::Value = match serde_json::from_str(&line) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        log::error!("Daemon I/O: failed to parse JSON: {}", e);
                                        continue;
                                    }
                                };

                                if raw.get("method").is_some() && raw.get("id").is_none() {
                                    let _ = event_tx_io.send(());
                                    continue;
                                }

                                if let Some(id) = raw.get("id").and_then(|v| v.as_u64()) {
                                    if let Some(tx) = pending.remove(&id) {
                                        let response: RpcResponse = match serde_json::from_value(raw) {
                                            Ok(r) => r,
                                            Err(e) => {
                                                log::error!("Daemon I/O: failed to parse response: {}", e);
                                                break;
                                            }
                                        };
                                        let _ = tx.send(response);
                                    } else {
                                        log::warn!(
                                            "Daemon I/O: received response for unknown id={}",
                                            id
                                        );
                                    }
                                } else {
                                    log::warn!("Daemon I/O: unexpected JSON format");
                                }
                            }
                            Err(e) => {
                                log::error!("Daemon I/O: failed to read daemon RPC socket: {}", e);
                                break;
                            }
                        }
                    }
                }
            }

            log::info!(
                "[lasper] I/O task exiting, waiting for child pid={}...",
                io_pid
            );
            drop(writer);
            let _ = child.wait().await;
            log::info!("[lasper] I/O task done (child reaped)");
        });

        // Health check
        let next_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let daemon = Self {
            request_tx,
            next_id,
            event_tx,
            pid,
            rpc_sock_path,
            fd_sock_path,
            fd_auth_token,
            _fd_sock_dir: fd_sock_dir,
            next_spawn_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            spawn_exit_codes: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
        };
        daemon.ping().await.map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("daemon health check failed after sudo launch: {}", error),
            )
        })?;

        log::info!("Elevated daemon ready (pid={})", pid);
        Ok(daemon)
    }

    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.event_tx.subscribe()
    }

    // ── JSON-RPC dispatch ──

    pub async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<serde_json::Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        log::trace!("[lasper] rpc_call: sending {} (id={})", method, id);
        self.request_tx
            .send((request, response_tx))
            .await
            .map_err(|_| {
                log::warn!("[lasper] rpc_call: mpsc send failed (I/O task stopped)");
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "daemon I/O task has stopped",
                )
            })?;
        log::trace!("[lasper] rpc_call: waiting for response id={}", id);
        let response = response_rx.await.map_err(|_| {
            log::warn!("[lasper] rpc_call: response channel cancelled id={}", id);
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "daemon response channel cancelled",
            )
        })?;
        if let Some(err) = response.error {
            return Err(std::io::Error::other(format!(
                "daemon error (code={}): {}",
                err.code, err.message
            )));
        }
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    }

    pub async fn ping(&self) -> std::io::Result<()> {
        self.rpc_call("ping", serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn exit(&self) {
        log::info!("[lasper] daemon::exit() sending RPC...");
        // Send exit command and ignore response since daemon will exit immediately
        let _ = self.rpc_call("exit", serde_json::json!({})).await;
        let _ = tokio::fs::remove_file(&self.rpc_sock_path).await;
        let _ = tokio::fs::remove_file(&self.fd_sock_path).await;
        let _ = tokio::fs::remove_dir(self._fd_sock_dir.path()).await;
        log::info!("[lasper] daemon::exit() RPC returned");
    }

    pub(crate) async fn nspawn_config(
        &self,
        operation: NspawnConfigOperation,
    ) -> std::io::Result<NspawnConfigResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("nspawn_config", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn system_operation(&self, operation: SystemOperation) -> std::io::Result<()> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc_call("system_operation", params).await?;
        Ok(())
    }

    pub(crate) async fn cli_inspect_machine(
        &self,
        name: &str,
    ) -> std::io::Result<MachineProperties> {
        let request = CliInspectMachineRequest {
            machine: MachineName::new(name)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        };
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("cli_inspect_machine", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn systemd_unit(
        &self,
        operation: SystemdUnitOperation,
    ) -> std::io::Result<SystemdUnitResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("systemd_unit", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn nvidia_state(
        &self,
        operation: NvidiaStateOperation,
    ) -> std::io::Result<NvidiaStateResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("nvidia_state", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn managed_storage(
        &self,
        operation: ManagedStorageOperation,
    ) -> std::io::Result<ManagedStorageResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("managed_storage", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn rootfs(&self, operation: RootfsOperation) -> std::io::Result<RootfsResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("rootfs", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn assess_tar_runtime(&self) -> std::io::Result<TarRuntimeAssessment> {
        let result = self
            .rpc_call("assess_tar_runtime", serde_json::json!({}))
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    // ── FD-passing ──
    //
    // Connect and write the request async, then hand the fd to a blocking
    // thread for recv_with_fd (sendfd on tokio::UnixStream uses try_io which
    // is unreliable when mixed with prior read/write ops on the same fd).

    pub async fn spawn_journalctl(&self, name: &str) -> std::io::Result<RawFd> {
        let name = MachineName::try_from(name)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let std_sock = self
            .open_fd_channel(FdOperation::Journalctl(SpawnJournalctlParams { name }))
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 256];
            let mut fds = [0i32 as RawFd; 1];
            let (n, fd_count) = std_sock.recv_with_fd(&mut buf, &mut fds)?;
            if fd_count > 0 {
                Ok(fds[0])
            } else {
                let msg = String::from_utf8_lossy(&buf[..n]);
                Err(std::io::Error::other(format!(
                    "daemon error: {}",
                    msg.trim()
                )))
            }
        })
        .await?
    }

    pub async fn spawn_terminal(
        &self,
        name: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<SpawnedTerminalPty> {
        let name = MachineName::try_from(name)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let size = TerminalSize::new(cols, rows)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let std_sock = self
            .open_fd_channel(FdOperation::Terminal(SpawnTerminalParams { name, size }))
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 256];
            let mut fds = [0i32 as RawFd; 1];
            let (n, fd_count) = std_sock.recv_with_fd(&mut buf, &mut fds)?;
            if fd_count > 0 {
                match serde_json::from_slice::<SpawnTerminalResponse>(&buf[..n]) {
                    Ok(response) => Ok(SpawnedTerminalPty {
                        master_fd: fds[0],
                        attach_kind: response.attach_kind,
                    }),
                    Err(error) => {
                        unsafe {
                            libc::close(fds[0]);
                        }
                        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                    }
                }
            } else {
                let msg = String::from_utf8_lossy(&buf[..n]);
                Err(std::io::Error::other(format!(
                    "daemon error: {}",
                    msg.trim()
                )))
            }
        })
        .await?
    }

    pub(crate) async fn import_raw_image(
        &self,
        machine: MachineName,
        source: std::fs::File,
    ) -> std::io::Result<()> {
        self.import_image_source(
            FdOperation::ImportRawImage(ImportRawImageParams { machine }),
            source,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn import_tar_image(
        &self,
        request: ImportTarRequest,
        source: std::fs::File,
    ) -> std::io::Result<ImageImportReport> {
        let warnings = self
            .import_image_source(FdOperation::ImportTarImage(request), source)
            .await?;
        Ok(ImageImportReport { warnings })
    }

    async fn import_image_source(
        &self,
        operation: FdOperation,
        source: std::fs::File,
    ) -> std::io::Result<Vec<String>> {
        let std_sock = self.open_fd_channel(operation).await?;
        tokio::task::spawn_blocking(move || {
            use std::io::BufRead;

            let mut reader = std::io::BufReader::new(std_sock.try_clone()?);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim() != "ready" {
                return Err(std::io::Error::other(format!(
                    "daemon refused image source fd: {}",
                    line.trim()
                )));
            }

            std_sock.send_with_fd(b"source", &[source.as_raw_fd()])?;
            line.clear();
            reader.read_line(&mut line)?;
            let response: ImportImageResponse = serde_json::from_str(line.trim())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            match response.error {
                Some(error) => Err(std::io::Error::other(error)),
                None => Ok(response.warnings),
            }
        })
        .await?
    }

    // ── Typed streaming operations ──

    /// Allocate a unique ID for a spawned command.
    pub fn reserve_spawn_id(&self) -> u64 {
        self.next_spawn_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) async fn spawn_bootstrap(
        &self,
        cmd_id: u64,
        request: BootstrapRequest,
    ) -> std::io::Result<RawFd> {
        let std_sock = self
            .open_fd_channel(FdOperation::Bootstrap(Box::new(SpawnBootstrapParams {
                cmd_id,
                request,
            })))
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 256];
            let mut fds = [0i32 as RawFd; 1];
            let (n, fd_count) = std_sock.recv_with_fd(&mut buf, &mut fds)?;
            if fd_count > 0 {
                Ok(fds[0])
            } else {
                let msg = String::from_utf8_lossy(&buf[..n]);
                Err(std::io::Error::other(format!(
                    "daemon error: {}",
                    msg.trim()
                )))
            }
        })
        .await?
    }

    pub(crate) async fn spawn_oci_pull(
        &self,
        cmd_id: u64,
        request: OciPullRequest,
    ) -> std::io::Result<RawFd> {
        let std_sock = self
            .open_fd_channel(FdOperation::OciPull(Box::new(SpawnOciPullParams {
                cmd_id,
                request,
            })))
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 256];
            let mut fds = [0i32 as RawFd; 1];
            let (n, fd_count) = std_sock.recv_with_fd(&mut buf, &mut fds)?;
            if fd_count > 0 {
                Ok(fds[0])
            } else {
                let msg = String::from_utf8_lossy(&buf[..n]);
                Err(std::io::Error::other(format!(
                    "daemon error: {}",
                    msg.trim()
                )))
            }
        })
        .await?
    }

    async fn open_fd_channel(
        &self,
        operation: FdOperation,
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        use tokio::io::AsyncWriteExt;

        let mut sock = tokio::net::UnixStream::connect(&self.fd_sock_path).await?;
        authorize_root_server(get_peer_credentials(&sock)?)?;
        let request = FdRequest {
            auth_token: self.fd_auth_token.to_string(),
            operation,
        };
        let mut request_line = serde_json::to_vec(&request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        request_line.push(b'\n');
        sock.write_all(&request_line).await?;

        let std_sock = sock.into_std()?;
        std_sock.set_nonblocking(false)?;
        Ok(std_sock)
    }

    /// Poll the daemon for the exit code of a spawned command.
    pub async fn wait_command(&self, cmd_id: u64) -> std::io::Result<i32> {
        let result = self
            .rpc_call("wait_command", serde_json::json!({"cmd_id": cmd_id}))
            .await?;
        result["exit_code"]
            .as_i64()
            .map(|c| c as i32)
            .ok_or_else(|| {
                std::io::Error::other("daemon: missing exit_code in wait_command response")
            })
    }

    pub async fn signal_command(&self, cmd_id: u64, signal: i32) -> std::io::Result<()> {
        let signal = match signal {
            libc::SIGTERM => "terminate",
            libc::SIGKILL => "kill",
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "unsupported command signal",
                ))
            }
        };
        self.rpc_call(
            "signal_command",
            serde_json::json!({"cmd_id": cmd_id, "signal": signal}),
        )
        .await?;
        Ok(())
    }
}

// ── Daemon main loop (child side) ──

struct AuthenticatedRpcConnection {
    reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    auth_token: Arc<str>,
    dbus_enabled: bool,
}

async fn accept_rpc_connection(
    listener: &UnixListener,
    expected_peer: PeerCredentials,
    auth_log: AuthLogLimiter,
) -> std::io::Result<AuthenticatedRpcConnection> {
    loop {
        let (stream, _) = listener.accept().await?;
        let actual_peer = match get_peer_credentials(&stream) {
            Ok(credentials) => credentials,
            Err(error) => {
                if let Some(message) = auth_log.record(format!(
                    "Daemon RPC: failed to read peer credentials: {error}"
                )) {
                    log::warn!("{message}");
                }
                continue;
            }
        };
        if let Err(error) = authorize_fd_peer(actual_peer, expected_peer) {
            if let Some(message) = auth_log.record(format!("Daemon RPC: rejected peer: {error:?}"))
            {
                log::warn!("{message}");
            }
            continue;
        }

        let (read_half, write_half) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let line = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_bounded_line(&mut reader, MAX_RPC_FRAME_BYTES),
        )
        .await
        {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                if let Some(message) =
                    auth_log.record("Daemon RPC: authentication handshake is missing".into())
                {
                    log::warn!("{message}");
                }
                continue;
            }
            Ok(Err(_)) | Err(_) => {
                if let Some(message) = auth_log.record(
                    "Daemon RPC: authentication handshake timed out or exceeded the frame limit"
                        .into(),
                ) {
                    log::warn!("{message}");
                }
                continue;
            }
        };
        let bootstrap: RpcBootstrap = match serde_json::from_str(&line) {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                if let Some(message) = auth_log.record(format!(
                    "Daemon RPC: invalid authentication handshake: {error}"
                )) {
                    log::warn!("{message}");
                }
                continue;
            }
        };
        if bootstrap.protocol_version != RPC_PROTOCOL_VERSION
            || uuid::Uuid::parse_str(&bootstrap.auth_token).is_err()
        {
            if let Some(message) =
                auth_log.record("Daemon RPC: authentication handshake rejected".into())
            {
                log::warn!("{message}");
            }
            continue;
        }
        return Ok(AuthenticatedRpcConnection {
            reader,
            writer: write_half,
            auth_token: Arc::from(bootstrap.auth_token),
            dbus_enabled: bootstrap.dbus_enabled,
        });
    }
}

pub async fn daemon_main(
    fd_sock_opt: Option<PathBuf>,
    rpc_sock_opt: Option<PathBuf>,
    user_uid: u32,
    expected_parent_pid: u32,
) -> ! {
    if let Err(error) = require_daemon_root(uzers::get_effective_uid()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
    crate::nspawn::sys::command::enable_daemon_child_lifecycle();
    if let Err(error) = initialize_daemon_logging() {
        eprintln!("lasper daemon: failed to initialize logging: {error}");
        std::process::exit(1);
    }
    if expected_parent_pid == 0 {
        log::error!("Daemon: missing launching TUI PID");
        std::process::exit(1);
    }
    let parent_pidfd = match open_pidfd(expected_parent_pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            log::error!("Daemon: failed to pin launching TUI process: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = monitor_parent(parent_pidfd) {
        log::error!("Daemon: failed to monitor launching TUI process: {error}");
        std::process::exit(1);
    }

    let expected_peer = PeerCredentials {
        pid: expected_parent_pid,
        uid: user_uid,
    };

    // ── Authenticated JSON-RPC Unix socket ──
    let rpc_sock_path = match rpc_sock_opt {
        Some(path) => path,
        None => {
            log::error!("Daemon: missing RPC socket path");
            std::process::exit(1);
        }
    };
    let rpc_listener = match UnixListener::bind(&rpc_sock_path) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("Daemon: failed to bind RPC socket: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = configure_user_socket(&rpc_sock_path, user_uid) {
        log::error!("Daemon: failed to secure RPC socket: {error}");
        std::process::exit(1);
    }

    // ── FD-passing Unix socket ──
    let sock_path = match fd_sock_opt {
        Some(path) => path,
        None => {
            log::error!("Daemon: missing fd socket path");
            std::process::exit(1);
        }
    };
    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Daemon: failed to bind: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = configure_user_socket(&sock_path, user_uid) {
        log::error!("Daemon: failed to secure fd socket: {}", e);
        std::process::exit(1);
    }

    let auth_log = AuthLogLimiter::default();
    let rpc = match accept_rpc_connection(&rpc_listener, expected_peer, auth_log.clone()).await {
        Ok(rpc) => rpc,
        Err(error) => {
            log::error!("Daemon: RPC authentication failed: {error}");
            std::process::exit(1);
        }
    };
    let fd_auth_token = rpc.auth_token;
    let dbus_enabled = rpc.dbus_enabled;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(32);
    let mut rpc_reader = rpc.reader;
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut writer = tokio::io::BufWriter::new(rpc.writer);
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        shutdown_daemon_resources().await;
        std::process::exit(0);
    });

    let dbus = initialize_dbus_backend(dbus_enabled).await;

    if let Some(ref dbus) = dbus {
        let out_tx_bg = out_tx.clone();
        let dbus_bg = dbus.clone();
        tokio::spawn(async move {
            let (ev_tx, mut ev_rx) =
                tokio::sync::mpsc::channel::<crate::nspawn::models::StatusUpdate>(16);
            tokio::spawn(async move {
                if let Err(e) = dbus_bg.watch_events(ev_tx).await {
                    log::error!("Daemon DBus watcher exited: {}", e);
                }
            });
            while ev_rx.recv().await.is_some() {
                let notif = serde_json::json!({"jsonrpc":"2.0","method":"dbus_event","params":{}});
                let line = serde_json::to_string(&notif).unwrap();
                if out_tx_bg.send(line).await.is_err() {
                    break;
                }
            }
        });
    }

    let fd_slots = Arc::new(tokio::sync::Semaphore::new(MAX_FD_CONNECTIONS));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let permit = match fd_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            if let Some(message) = auth_log
                                .record("Daemon fd-handler: connection limit reached".into())
                            {
                                log::warn!("{message}");
                            }
                            continue;
                        }
                    };
                    tokio::spawn(handle_fd_connection(
                        stream,
                        expected_peer,
                        fd_auth_token.clone(),
                        auth_log.clone(),
                        permit,
                    ));
                }
                Err(e) => {
                    log::error!("Daemon fd listener: {}", e);
                    break;
                }
            }
        }
    });

    // ── Main request loop ──
    async fn send_or_exit(out_tx: &tokio::sync::mpsc::Sender<String>, json: serde_json::Value) {
        let line = serde_json::to_string(&json).unwrap();
        if out_tx.send(line).await.is_err() {
            shutdown_daemon_resources().await;
            std::process::exit(0);
        }
    }

    loop {
        let line = match read_bounded_line(&mut rpc_reader, MAX_RPC_FRAME_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                shutdown_daemon_resources().await;
                std::process::exit(0);
            }
            Err(e) => {
                log::error!("Daemon RPC socket error: {}", e);
                shutdown_daemon_resources().await;
                std::process::exit(1);
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: RpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                send_or_exit(
                    &out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":null,
                        "error":{"code":-32700,"message":format!("Parse error: {}", e)}
                    }),
                )
                .await;
                continue;
            }
        };

        match handle_request(&request, &dbus, &out_tx, user_uid).await {
            HandleOutcome::Spawned => {}
            HandleOutcome::Sync(Ok(result)) => {
                send_or_exit(
                    &out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":request.id,"result":result
                    }),
                )
                .await;
            }
            HandleOutcome::Sync(Err(e)) => {
                send_or_exit(
                    &out_tx,
                    serde_json::json!({
                        "jsonrpc":"2.0","id":request.id,"error":{"code":-1,"message":e}
                    }),
                )
                .await;
            }
        }
    }
}

async fn handle_request<B: DaemonDbusExecutor>(
    request: &RpcRequest,
    dbus: &Option<B>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    invoking_uid: u32,
) -> HandleOutcome {
    match request.method.as_str() {
        "ping" => HandleOutcome::Sync(Ok(serde_json::Value::Null)),

        "nspawn_config" => {
            let operation: NspawnConfigOperation =
                match serde_json::from_value(request.params.clone()) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid nspawn_config request: {error}"
                        )));
                    }
                };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_nspawn_config_operation(operation, invoking_uid).await
                {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "systemd_unit" => {
            let operation: SystemdUnitOperation =
                match serde_json::from_value(request.params.clone()) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid systemd_unit request: {error}"
                        )));
                    }
                };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_systemd_unit_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "nvidia_state" => {
            let operation: NvidiaStateOperation =
                match serde_json::from_value(request.params.clone()) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid nvidia_state request: {error}"
                        )));
                    }
                };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_nvidia_state_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "managed_storage" => {
            let operation: ManagedStorageOperation =
                match serde_json::from_value(request.params.clone()) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid managed_storage request: {error}"
                        )));
                    }
                };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_managed_storage_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "rootfs" => {
            let operation: RootfsOperation = match serde_json::from_value(request.params.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!("invalid rootfs request: {error}")));
                }
            };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_rootfs_operation(operation).await {
                    Ok(result) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }})
                    }
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "assess_tar_runtime" => {
            if request.params != serde_json::json!({}) {
                return HandleOutcome::Sync(Err(
                    "assess_tar_runtime does not accept parameters".into()
                ));
            }
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(inspect_tar_runtime).await;
                let response = match result {
                    Ok(Ok(assessment)) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":assessment})
                    }
                    Ok(Err(error)) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":error.to_string(),
                    }}),
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":format!("tar runtime inspection task failed: {error}"),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "system_operation" => {
            let operation: SystemOperation = match serde_json::from_value(request.params.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid system_operation request: {error}"
                    )));
                }
            };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match execute_system_operation(operation).await {
                    Ok(()) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":error.to_string(),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "cli_inspect_machine" => {
            let inspection: CliInspectMachineRequest =
                match serde_json::from_value(request.params.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid cli_inspect_machine request: {error}"
                        )));
                    }
                };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match crate::nspawn::adapters::comm::cli::get_properties_with_runner(
                    inspection.machine.as_str(),
                    &crate::nspawn::sys::command::DefaultCommandRunner,
                )
                .await
                {
                    Ok(properties) => match serde_json::to_value(properties) {
                        Ok(result) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                        Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-1,
                            "message":error.to_string(),
                        }}),
                    },
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":-1,
                        "message":error.to_string(),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        "dbus_list_machines" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_machines().await {
                Ok(machines) => match serde_json::to_value(machines) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_list_images" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_images().await {
                Ok(images) => match serde_json::to_value(images) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_system_operation" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let operation: SystemOperation = match serde_json::from_value(request.params.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    return HandleOutcome::Sync(Err(format!(
                        "invalid dbus_system_operation request: {error}"
                    )));
                }
            };
            match dbus.system_operation(operation).await {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_get_properties" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let name = match request_machine_name(request) {
                Ok(name) => name,
                Err(error) => return HandleOutcome::Sync(Err(error)),
            };
            match dbus.get_properties(name.as_str()).await {
                Ok(props) => match serde_json::to_value(props) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_is_available" => match dbus {
            Some(dbus) => {
                HandleOutcome::Sync(Ok(serde_json::Value::Bool(dbus.is_available().await)))
            }
            None => HandleOutcome::Sync(Ok(serde_json::Value::Bool(false))),
        },

        "wait_command" => {
            let cmd_id = match request.params["cmd_id"].as_u64() {
                Some(id) => id,
                None => return HandleOutcome::Sync(Err("missing cmd_id".into())),
            };
            let id = request.id;
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                loop {
                    let error = SPAWN_WAIT_ERRORS.lock().remove(&cmd_id);
                    if let Some(error) = error {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -1, "message": error},
                        });
                        let line = serde_json::to_string(&response).unwrap();
                        let _ = out_tx.send(line).await;
                        return;
                    }
                    let code = SPAWN_EXIT_CODES.lock().remove(&cmd_id);
                    if let Some(code) = code {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"exit_code": code},
                        });
                        let line = serde_json::to_string(&response).unwrap();
                        let _ = out_tx.send(line).await;
                        return;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            });
            HandleOutcome::Spawned
        }

        "signal_command" => {
            let Some(cmd_id) = request.params["cmd_id"].as_u64() else {
                return HandleOutcome::Sync(Err("missing cmd_id".into()));
            };
            let signal = match request.params["signal"].as_str() {
                Some("terminate") => libc::SIGTERM,
                Some("kill") => libc::SIGKILL,
                _ => return HandleOutcome::Sync(Err("unsupported command signal".into())),
            };
            if let Some(cancellation) = OCI_TRANSFER_CANCELLATIONS.lock().get(&cmd_id).cloned() {
                cancellation.request();
                return HandleOutcome::Sync(Ok(serde_json::Value::Null));
            }
            let Some(pid) = SPAWN_PIDS.lock().get(&cmd_id).copied() else {
                return HandleOutcome::Sync(Ok(serde_json::Value::Null));
            };
            let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
            let error = (result != 0).then(std::io::Error::last_os_error);
            if result == 0
                || error.as_ref().and_then(std::io::Error::raw_os_error) == Some(libc::ESRCH)
            {
                HandleOutcome::Sync(Ok(serde_json::Value::Null))
            } else {
                HandleOutcome::Sync(Err(format!(
                    "failed to signal command {cmd_id}: {}",
                    error.expect("failed kill has an OS error")
                )))
            }
        }

        "exit" => {
            shutdown_daemon_resources().await;
            std::process::exit(0);
        }

        _ => HandleOutcome::Sync(Err(format!("unknown method: {}", request.method))),
    }
}

fn request_machine_name(request: &RpcRequest) -> Result<MachineName, String> {
    let name = request.params["name"]
        .as_str()
        .ok_or_else(|| "missing name".to_string())?;
    MachineName::try_from(name).map_err(|error| error.to_string())
}

// ── Peer credential verification ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerCredentials {
    pid: u32,
    uid: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum FdAuthorizationError {
    UnexpectedUid { actual: u32, expected: u32 },
    UnexpectedPid { actual: u32, expected: u32 },
    InvalidToken,
}

/// Returns the PID and UID of the process on the other end of a Unix socket.
/// Uses `SO_PEERCRED` — the kernel fills in the credentials, so they
/// cannot be forged by the connecting process.
fn get_peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pid = u32::try_from(ucred.pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "peer reported an invalid PID",
        )
    })?;
    Ok(PeerCredentials {
        pid,
        uid: ucred.uid,
    })
}

fn authorize_fd_peer(
    actual: PeerCredentials,
    expected: PeerCredentials,
) -> Result<(), FdAuthorizationError> {
    if actual.uid != expected.uid {
        return Err(FdAuthorizationError::UnexpectedUid {
            actual: actual.uid,
            expected: expected.uid,
        });
    }
    if actual.pid != expected.pid {
        return Err(FdAuthorizationError::UnexpectedPid {
            actual: actual.pid,
            expected: expected.pid,
        });
    }
    Ok(())
}

fn authorize_root_server(actual: PeerCredentials) -> std::io::Result<()> {
    if actual.uid == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "elevated daemon socket peer has uid {} instead of root (pid {})",
                actual.uid, actual.pid
            ),
        ))
    }
}

fn authorize_fd_token(actual: &str, expected: &str) -> Result<(), FdAuthorizationError> {
    if actual.len() != expected.len() {
        return Err(FdAuthorizationError::InvalidToken);
    }

    let difference = actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0u8, |difference, (actual, expected)| {
            difference | (actual ^ expected)
        });
    if difference == 0 {
        Ok(())
    } else {
        Err(FdAuthorizationError::InvalidToken)
    }
}

// ── Daemon side fd-passing handler ──

async fn handle_fd_connection(
    stream: UnixStream,
    expected_peer: PeerCredentials,
    expected_auth_token: Arc<str>,
    auth_log: AuthLogLimiter,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let actual_peer = match get_peer_credentials(&stream) {
        Ok(credentials) => credentials,
        Err(e) => {
            if let Some(message) =
                auth_log.record(format!("Daemon fd-handler: SO_PEERCRED failed: {e}"))
            {
                log::warn!("{message}");
            }
            return;
        }
    };
    if let Err(e) = authorize_fd_peer(actual_peer, expected_peer) {
        let message = match e {
            FdAuthorizationError::UnexpectedUid { actual, expected } => {
                format!("Daemon fd-handler: rejected uid {actual} (expected {expected})")
            }
            FdAuthorizationError::UnexpectedPid { actual, expected } => {
                format!("Daemon fd-handler: rejected pid {actual} (expected {expected})")
            }
            FdAuthorizationError::InvalidToken => unreachable!(),
        };
        if let Some(message) = auth_log.record(message) {
            log::warn!("{message}");
        }
        return;
    }

    let mut buf_reader = tokio::io::BufReader::new(stream);
    let line = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_bounded_line(&mut buf_reader, MAX_RPC_FRAME_BYTES),
    )
    .await
    {
        Ok(Ok(Some(line))) if !line.trim().is_empty() => line,
        _ => {
            if let Some(message) =
                auth_log.record("Daemon fd-handler: failed to read request line".into())
            {
                log::warn!("{message}");
            }
            return;
        }
    };

    let request: FdRequest = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            if let Some(message) = auth_log.record(format!("Daemon fd-handler: parse error: {e}")) {
                log::warn!("{message}");
            }
            return;
        }
    };
    if authorize_fd_token(&request.auth_token, &expected_auth_token).is_err() {
        if let Some(message) = auth_log.record(format!(
            "Daemon fd-handler: rejected unauthenticated request from pid {}",
            actual_peer.pid
        )) {
            log::warn!("{message}");
        }
        return;
    }

    let operation = request.operation;
    let stream = buf_reader.into_inner();

    // Convert to std stream for blocking send_with_fd.
    // tokio streams are non-blocking — must switch back so sendmsg
    // doesn't return EAGAIN.
    let mut std_stream = match stream.into_std() {
        Ok(s) => {
            let _ = s.set_nonblocking(false);
            s
        }
        Err(e) => {
            log::error!("Daemon fd-handler: into_std failed: {}", e);
            return;
        }
    };

    match operation {
        FdOperation::Journalctl(SpawnJournalctlParams { name }) => {
            match crate::nspawn::sys::new_sync_command("journalctl")
                .args([
                    "-M",
                    name.as_str(),
                    "-n",
                    "1000",
                    "-f",
                    "--no-pager",
                    "--output=short",
                ])
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn()
            {
                Ok(mut child) => {
                    let child_pid = child.id();
                    DAEMON_SESSION_PIDS
                        .lock()
                        .insert(child_pid as u64, child_pid);
                    let stdout = child.stdout.take().expect("stdout piped");
                    let raw_fd = stdout.as_raw_fd();

                    if let Err(e) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (journalctl) failed: {}", e);
                        let _ = crate::nspawn::sys::command::signal_process_group(
                            child_pid,
                            libc::SIGKILL,
                        );
                    }
                    drop(stdout);
                    tokio::task::spawn_blocking(move || {
                        let _ = child.wait();
                        DAEMON_SESSION_PIDS.lock().remove(&(child_pid as u64));
                    });
                }
                Err(e) => {
                    log::error!("Daemon: spawn journalctl failed: {}", e);
                }
            }
        }

        FdOperation::Terminal(SpawnTerminalParams { name, size }) => {
            use portable_pty::{native_pty_system, PtySize};
            let pty_system = native_pty_system();
            let pair = match pty_system.openpty(PtySize {
                rows: size.rows(),
                cols: size.cols(),
                pixel_width: 0,
                pixel_height: 0,
            }) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Daemon: openpty failed: {}", e);
                    let _ = std_stream.send_with_fd(e.to_string().as_bytes(), &[]);
                    return;
                }
            };

            let attach = match crate::nspawn::sys::terminal_attach::select(&name) {
                Ok(attach) => attach,
                Err(e) => {
                    log::error!("Daemon: terminal attach planning failed: {}", e);
                    let _ = std_stream.send_with_fd(e.to_string().as_bytes(), &[]);
                    return;
                }
            };
            let attach_kind = attach.kind();
            let cmd = attach.into_pty_command();
            let terminal_name = name.to_string();
            match pair.slave.spawn_command(cmd) {
                Ok(mut child) => {
                    drop(pair.slave);
                    let child_pid = child.process_id();
                    if let Some(child_pid) = child_pid {
                        DAEMON_SESSION_PIDS
                            .lock()
                            .insert(child_pid as u64, child_pid);
                    }
                    let master_fd = pair.master.as_raw_fd().expect("master has fd");
                    let response = serde_json::to_vec(&SpawnTerminalResponse { attach_kind })
                        .expect("terminal response is serializable");

                    if let Err(e) = std_stream.send_with_fd(&response, &[master_fd]) {
                        log::error!("Daemon: send_with_fd (terminal) failed: {}", e);
                        if let Some(child_pid) = child_pid {
                            let _ = crate::nspawn::sys::command::signal_process_group(
                                child_pid,
                                libc::SIGKILL,
                            );
                        }
                    }
                    drop(pair.master);
                    tokio::task::spawn_blocking(move || {
                        match child.wait() {
                            Ok(status) if status.success() => log::info!(
                                "Terminal attachment for {} ({:?}) exited normally",
                                terminal_name,
                                attach_kind
                            ),
                            Ok(status) => log::warn!(
                                "Terminal attachment for {} ({:?}) exited with code {} signal {:?}",
                                terminal_name,
                                attach_kind,
                                status.exit_code(),
                                status.signal()
                            ),
                            Err(error) => log::warn!(
                                "Failed to wait for terminal attachment to {} ({:?}): {}",
                                terminal_name,
                                attach_kind,
                                error
                            ),
                        }
                        if let Some(child_pid) = child_pid {
                            DAEMON_SESSION_PIDS.lock().remove(&(child_pid as u64));
                        }
                    });
                }
                Err(e) => {
                    log::error!("Daemon: spawn terminal attachment failed: {}", e);
                    let _ = std_stream.send_with_fd(e.to_string().as_bytes(), &[]);
                }
            }
        }

        FdOperation::Bootstrap(params) => {
            let SpawnBootstrapParams { cmd_id, request } = *params;
            if let Err(error) = validate_bootstrap_target(&request.target).await {
                log::warn!("Daemon: rejected bootstrap target: {}", error);
                let _ = std_stream.send_with_fd(error.to_string().as_bytes(), &[]);
                return;
            }
            let signature_style = match probe_debootstrap_signature_style_sync(&request) {
                Ok(style) => style,
                Err(error) => {
                    log::warn!("Daemon: debootstrap capability probe failed: {}", error);
                    let _ = std_stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            let (program, args) = match build_bootstrap_command(&request, signature_style) {
                Ok(command) => command,
                Err(error) => {
                    log::warn!("Daemon: rejected bootstrap request: {}", error);
                    let _ = std_stream.send_with_fd(error.to_string().as_bytes(), &[]);
                    return;
                }
            };
            log::info!(
                "[AUDIT] [Step: Bootstrap] Starting typed {} operation",
                program
            );
            let mut command = crate::nspawn::sys::new_sync_command("sh");
            command
                .arg("-c")
                .arg("exec \"$@\" 2>&1")
                .arg("--")
                .arg(&program)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .process_group(0);
            match command.spawn() {
                Ok(mut child) => {
                    SPAWN_PIDS.lock().insert(cmd_id, child.id());
                    let stdout = child.stdout.take().expect("stdout piped");
                    let raw_fd = stdout.as_raw_fd();
                    if let Err(error) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (spawn_bootstrap) failed: {}", error);
                        drop(stdout);
                        stop_unhanded_command(&mut child, cmd_id, "bootstrap");
                        return;
                    }
                    drop(stdout);

                    tokio::task::spawn_blocking(move || {
                        let status = child.wait();
                        SPAWN_PIDS.lock().remove(&cmd_id);
                        if let Ok(status) = status {
                            SPAWN_EXIT_CODES.lock().insert(cmd_id, status.into_raw());
                        }
                    });
                }
                Err(error) => {
                    log::error!("Daemon: typed bootstrap spawn failed: {}", error);
                    let _ = std_stream.send_with_fd(b"bootstrap spawn failed", &[]);
                }
            }
        }

        FdOperation::OciPull(params) => {
            let SpawnOciPullParams { cmd_id, request } = *params;
            log::info!(
                "[AUDIT] [Step: OCI] Starting typed systemd-importd PullOci transfer for {}",
                request.machine
            );
            let (writer, reader) = match tokio::net::unix::pipe::pipe() {
                Ok(pipe) => pipe,
                Err(error) => {
                    log::error!("Daemon: OCI transfer pipe creation failed: {error}");
                    let _ = std_stream.send_with_fd(b"OCI transfer pipe failed", &[]);
                    return;
                }
            };
            let cancellation = OciTransferCancellation::default();
            OCI_TRANSFER_CANCELLATIONS
                .lock()
                .insert(cmd_id, cancellation.clone());
            if let Err(error) = std_stream.send_with_fd(b"ok", &[reader.as_raw_fd()]) {
                log::error!("Daemon: send_with_fd (spawn_oci_pull) failed: {error}");
                OCI_TRANSFER_CANCELLATIONS.lock().remove(&cmd_id);
                return;
            }
            drop(reader);

            tokio::spawn(async move {
                let result = run_oci_transfer(request, writer, cancellation).await;
                OCI_TRANSFER_CANCELLATIONS.lock().remove(&cmd_id);
                match result {
                    Ok(outcome) => {
                        if let OciTransferOutcome::Failed(reason) = &outcome {
                            log::error!("systemd-importd OCI transfer failed: {reason}");
                        }
                        SPAWN_EXIT_CODES
                            .lock()
                            .insert(cmd_id, outcome.exit_code() << 8);
                    }
                    Err(error) => {
                        SPAWN_WAIT_ERRORS.lock().insert(
                            cmd_id,
                            format!("could not confirm systemd-importd transfer state: {error}"),
                        );
                    }
                }
            });
        }

        FdOperation::ImportRawImage(ImportRawImageParams { machine }) => {
            let source = match receive_image_source(&mut std_stream) {
                Ok(source) => source,
                Err(error) => {
                    send_image_import_response(&mut std_stream, Err(error));
                    return;
                }
            };

            let result = crate::nspawn::ops::provision::image_operation::import_raw_system_image(
                machine, source,
            )
            .await
            .map(|_| ImageImportReport::default())
            .map_err(|error| error.to_string());
            send_image_import_response(&mut std_stream, result);
        }

        FdOperation::ImportTarImage(request) => {
            let source = match receive_image_source(&mut std_stream) {
                Ok(source) => source,
                Err(error) => {
                    send_image_import_response(&mut std_stream, Err(error));
                    return;
                }
            };
            let result =
                crate::nspawn::ops::provision::image_operation::import_tar_image(request, source)
                    .await
                    .map_err(|error| error.to_string());
            send_image_import_response(&mut std_stream, result);
        }
    }
}

fn receive_image_source(
    stream: &mut std::os::unix::net::UnixStream,
) -> std::result::Result<std::fs::File, String> {
    use std::io::Write;
    use std::os::fd::FromRawFd;

    stream
        .write_all(b"ready\n")
        .map_err(|error| format!("failed to acknowledge image source fd: {error}"))?;
    let mut marker = [0u8; 16];
    let mut fds = [0i32 as RawFd; 1];
    match stream.recv_with_fd(&mut marker, &mut fds) {
        Ok((_, 1)) => Ok(unsafe { std::fs::File::from_raw_fd(fds[0]) }),
        Ok((_, count)) => Err(format!(
            "expected exactly one image source fd, received {count}"
        )),
        Err(error) => Err(format!("failed to receive image source fd: {error}")),
    }
}

fn send_image_import_response(
    stream: &mut std::os::unix::net::UnixStream,
    result: std::result::Result<ImageImportReport, String>,
) {
    use std::io::Write;

    let response = image_import_response(result);
    if let Ok(line) = serde_json::to_string(&response) {
        let _ = stream.write_all(format!("{line}\n").as_bytes());
    }
}

fn image_import_response(
    result: std::result::Result<ImageImportReport, String>,
) -> ImportImageResponse {
    match result {
        Ok(report) => ImportImageResponse {
            warnings: report.warnings,
            error: None,
        },
        Err(error) => ImportImageResponse {
            warnings: Vec::new(),
            error: Some(error),
        },
    }
}

static SPAWN_EXIT_CODES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, i32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

static SPAWN_WAIT_ERRORS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

static SPAWN_PIDS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<u64, u32>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

static OCI_TRANSFER_CANCELLATIONS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, OciTransferCancellation>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

static DAEMON_SESSION_PIDS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, u32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

async fn shutdown_daemon_resources() {
    for cancellation in OCI_TRANSFER_CANCELLATIONS.lock().values() {
        cancellation.request();
    }

    let mut pids = std::collections::HashSet::new();
    pids.extend(SPAWN_PIDS.lock().values().copied());
    pids.extend(DAEMON_SESSION_PIDS.lock().values().copied());
    for pid in &pids {
        if let Err(error) = crate::nspawn::sys::command::signal_process_group(*pid, libc::SIGTERM) {
            log::warn!("Daemon: failed to terminate child process group {pid}: {error}");
        }
    }

    tokio::time::sleep(DAEMON_SHUTDOWN_GRACE).await;

    let mut remaining = std::collections::HashSet::new();
    remaining.extend(SPAWN_PIDS.lock().values().copied());
    remaining.extend(DAEMON_SESSION_PIDS.lock().values().copied());
    for pid in remaining {
        if let Err(error) = crate::nspawn::sys::command::signal_process_group(pid, libc::SIGKILL) {
            log::warn!("Daemon: failed to kill child process group {pid}: {error}");
        }
    }
}

fn stop_unhanded_command(child: &mut std::process::Child, cmd_id: u64, label: &str) {
    let pid = child.id();
    if let Err(error) = crate::nspawn::sys::command::signal_process_group(pid, libc::SIGKILL) {
        log::error!("Daemon: failed to stop unhanded {label} command {cmd_id}: {error}");
    }
    if let Err(error) = child.wait() {
        log::error!("Daemon: failed to wait for unhanded {label} command {cmd_id}: {error}");
    }
    SPAWN_PIDS.lock().remove(&cmd_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "f865fd7e-a9f5-4ef1-b5b5-f3f257a75ce0";

    #[derive(Clone)]
    struct SlowRemoveDbus {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl DaemonDbusExecutor for SlowRemoveDbus {
        async fn list_machines(&self) -> crate::nspawn::errors::Result<Vec<ContainerEntry>> {
            Err(crate::nspawn::errors::NspawnError::Runtime(
                "slow test backend does not list machines".into(),
            ))
        }

        async fn list_images(&self) -> crate::nspawn::errors::Result<Vec<ImageEntry>> {
            Err(crate::nspawn::errors::NspawnError::Runtime(
                "slow test backend does not list images".into(),
            ))
        }

        async fn system_operation(
            &self,
            operation: SystemOperation,
        ) -> crate::nspawn::errors::Result<()> {
            match operation {
                SystemOperation::RemoveImage { .. } => {
                    self.started.notify_one();
                    self.release.notified().await;
                    Ok(())
                }
                _ => Err(crate::nspawn::errors::NspawnError::Runtime(
                    "slow test backend only handles image removal".into(),
                )),
            }
        }

        async fn get_properties(
            &self,
            _name: &str,
        ) -> crate::nspawn::errors::Result<MachineProperties> {
            Err(crate::nspawn::errors::NspawnError::Runtime(
                "slow test backend does not inspect machines".into(),
            ))
        }

        async fn is_available(&self) -> bool {
            true
        }
    }

    #[test]
    fn unhanded_command_is_stopped_and_reaped() {
        let cmd_id = u64::MAX;
        let mut command = crate::nspawn::sys::new_sync_command("sh");
        command.args(["-c", "exec sleep 30"]).process_group(0);
        let mut child = command.spawn().unwrap();
        SPAWN_PIDS.lock().insert(cmd_id, child.id());

        stop_unhanded_command(&mut child, cmd_id, "test");

        assert!(child.try_wait().unwrap().is_some());
        assert!(!SPAWN_PIDS.lock().contains_key(&cmd_id));
    }

    #[test]
    fn rpc_bootstrap_carries_transport_mode_and_session_secret() {
        let bootstrap = RpcBootstrap {
            protocol_version: RPC_PROTOCOL_VERSION,
            auth_token: TEST_TOKEN.to_string(),
            dbus_enabled: false,
        };
        let json = serde_json::to_value(&bootstrap).unwrap();
        let parsed: RpcBootstrap = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.auth_token, TEST_TOKEN);
        assert!(!parsed.dbus_enabled);

        let invalid_token = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
            "auth_token": "not-a-uuid",
            "dbus_enabled": false,
        });
        let parsed: RpcBootstrap = serde_json::from_value(invalid_token).unwrap();
        assert!(uuid::Uuid::parse_str(&parsed.auth_token).is_err());

        let unknown_field = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
            "auth_token": TEST_TOKEN,
            "dbus_enabled": false,
            "unexpected": true,
        });
        assert!(serde_json::from_value::<RpcBootstrap>(unknown_field).is_err());

        let missing_mode = serde_json::json!({
            "protocol_version": RPC_PROTOCOL_VERSION,
            "auth_token": TEST_TOKEN,
        });
        assert!(serde_json::from_value::<RpcBootstrap>(missing_mode).is_err());
    }

    #[test]
    fn rpc_request_rejects_unknown_fields() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {},
            "auth_token": TEST_TOKEN,
        });
        assert!(serde_json::from_value::<RpcRequest>(request).is_err());
    }

    #[tokio::test]
    async fn rpc_handshake_preserves_the_first_request_after_authentication() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("rpc.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let expected_peer = PeerCredentials {
            pid: std::process::id(),
            uid: uzers::get_current_uid(),
        };
        let server = tokio::spawn(async move {
            accept_rpc_connection(&listener, expected_peer, AuthLogLimiter::default())
                .await
                .unwrap()
        });

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let authentication = RpcBootstrap {
            protocol_version: RPC_PROTOCOL_VERSION,
            auth_token: TEST_TOKEN.to_string(),
            dbus_enabled: true,
        };
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        client
            .write_all(
                format!(
                    "{}\n{request}\n",
                    serde_json::to_string(&authentication).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let connection = server.await.unwrap();
        assert!(connection.dbus_enabled);
        assert_eq!(connection.auth_token.as_ref(), TEST_TOKEN);
        let mut lines = connection.reader.lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some(request));
    }

    #[tokio::test]
    async fn bounded_protocol_reader_preserves_following_frames() {
        let input = &b"first\nsecond\n"[..];
        let mut reader = tokio::io::BufReader::new(input);

        assert_eq!(
            read_bounded_line(&mut reader, 16).await.unwrap().as_deref(),
            Some("first\n")
        );
        assert_eq!(
            read_bounded_line(&mut reader, 16).await.unwrap().as_deref(),
            Some("second\n")
        );
        assert!(read_bounded_line(&mut reader, 16).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bounded_protocol_reader_rejects_oversized_frames() {
        let input = &b"123456789\n"[..];
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(&mut reader, 8).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn cli_mode_skips_daemon_dbus_initialization() {
        assert!(initialize_dbus_backend(false).await.is_none());
    }

    #[tokio::test]
    async fn slow_remove_image_reproduces_inline_pump_blocking() {
        if std::env::var_os("LASPER_RUN_SLOW_REPRODUCER").is_none() {
            return;
        }

        let slow = SlowRemoveDbus {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        let started = slow.started.clone();
        let release = slow.release.clone();
        let dbus = Some(slow);
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
        let remove = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "dbus_system_operation".into(),
            params: serde_json::to_value(SystemOperation::RemoveImage {
                image: crate::nspawn::models::ImageName::new("slow-image").unwrap(),
            })
            .unwrap(),
        };
        let ping = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 2,
            method: "ping".into(),
            params: serde_json::json!({}),
        };

        // This mirrors the current daemon loop: one dispatch future is
        // awaited before the next frame is read. The test is intentionally
        // env-gated until Phase 1 replaces this assertion with responsiveness.
        let mut pump = tokio::spawn(async move {
            let remove_outcome =
                handle_request(&remove, &dbus, &out_tx, uzers::get_current_uid()).await;
            let ping_outcome =
                handle_request(&ping, &dbus, &out_tx, uzers::get_current_uid()).await;
            (remove_outcome, ping_outcome)
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
            .await
            .expect("RemoveImage did not reach the slow executor");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut pump)
                .await
                .is_err()
        );

        release.notify_one();
        let (remove_outcome, ping_outcome) =
            tokio::time::timeout(std::time::Duration::from_secs(1), pump)
                .await
                .expect("pump did not resume after RemoveImage completed")
                .expect("pump task panicked");
        assert!(matches!(
            remove_outcome,
            HandleOutcome::Sync(Ok(serde_json::Value::Null))
        ));
        assert!(matches!(
            ping_outcome,
            HandleOutcome::Sync(Ok(serde_json::Value::Null))
        ));
    }

    #[test]
    fn cli_inspection_rpc_accepts_only_a_typed_machine_name() {
        let valid: CliInspectMachineRequest =
            serde_json::from_value(serde_json::json!({"machine": "test-machine"})).unwrap();
        assert_eq!(valid.machine.as_str(), "test-machine");

        assert!(serde_json::from_value::<CliInspectMachineRequest>(
            serde_json::json!({"machine": "../escape"})
        )
        .is_err());
        assert!(serde_json::from_value::<CliInspectMachineRequest>(
            serde_json::json!({"machine": "test-machine", "unexpected": true})
        )
        .is_err());
    }

    #[test]
    fn fd_peer_rejects_another_process_with_same_uid() {
        let expected = PeerCredentials {
            pid: 1000,
            uid: 1000,
        };
        let actual = PeerCredentials {
            pid: 1001,
            uid: 1000,
        };

        assert_eq!(
            authorize_fd_peer(actual, expected),
            Err(FdAuthorizationError::UnexpectedPid {
                actual: 1001,
                expected: 1000,
            })
        );
    }

    #[test]
    fn fd_peer_rejects_unexpected_uid() {
        let expected = PeerCredentials {
            pid: 1000,
            uid: 1000,
        };
        let actual = PeerCredentials {
            pid: 1000,
            uid: 1001,
        };

        assert_eq!(
            authorize_fd_peer(actual, expected),
            Err(FdAuthorizationError::UnexpectedUid {
                actual: 1001,
                expected: 1000,
            })
        );
    }

    #[test]
    fn daemon_root_check_rejects_non_root_effective_uid() {
        assert!(require_daemon_root(0).is_ok());
        let error = require_daemon_root(1000).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn rpc_client_requires_a_root_server_peer() {
        assert!(authorize_root_server(PeerCredentials { pid: 1, uid: 0 }).is_ok());
        let error = authorize_root_server(PeerCredentials { pid: 42, uid: 1000 }).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn fd_token_requires_exact_session_secret() {
        assert_eq!(authorize_fd_token(TEST_TOKEN, TEST_TOKEN), Ok(()));
        assert_eq!(
            authorize_fd_token("f865fd7e-a9f5-4ef1-b5b5-f3f257a75ce1", TEST_TOKEN),
            Err(FdAuthorizationError::InvalidToken)
        );
        assert_eq!(
            authorize_fd_token("short", TEST_TOKEN),
            Err(FdAuthorizationError::InvalidToken)
        );
    }

    #[test]
    fn fd_request_without_authentication_is_rejected_by_parser() {
        let request = r#"{"method":"spawn_oci_pull","params":{"cmd_id":1,"request":{"reference":"nginx","machine":"test","read_only":false}}}"#;
        assert!(serde_json::from_str::<FdRequest>(request).is_err());
    }

    #[test]
    fn fd_request_round_trip_uses_typed_terminal_parameters() {
        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::Terminal(SpawnTerminalParams {
                name: MachineName::new("test-machine").unwrap(),
                size: TerminalSize::new(120, 40).unwrap(),
            }),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "spawn_terminal");
        assert_eq!(json["params"]["name"], "test-machine");
        assert_eq!(json["params"]["size"]["cols"], 120);
        assert_eq!(json["params"]["size"]["rows"], 40);

        let parsed: FdRequest = serde_json::from_value(json).unwrap();
        match parsed.operation {
            FdOperation::Terminal(params) => {
                assert_eq!(params.name.as_str(), "test-machine");
                assert_eq!(params.size, TerminalSize::new(120, 40).unwrap());
            }
            _ => panic!("expected spawn_terminal"),
        }
    }

    #[test]
    fn fd_request_round_trip_uses_typed_bootstrap_parameters() {
        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::Bootstrap(Box::new(SpawnBootstrapParams {
                cmd_id: 7,
                request: BootstrapRequest {
                    target: crate::nspawn::adapters::rootfs::RootfsTarget::Machine {
                        machine: MachineName::new("test-machine").unwrap(),
                    },
                    spec: crate::nspawn::models::BootstrapSpec::Debootstrap(
                        crate::nspawn::models::DebootstrapSpec::default(),
                    ),
                    include_sudo: true,
                },
            })),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "spawn_bootstrap");
        assert_eq!(json["params"]["cmd_id"], 7);
        assert_eq!(json["params"]["request"]["target"]["kind"], "machine");
        assert_eq!(json["params"]["request"]["spec"]["provider"], "debootstrap");
        assert!(json["params"].get("program").is_none());
        assert!(json["params"].get("args").is_none());

        let parsed: FdRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed.operation, FdOperation::Bootstrap(_)));
    }

    #[test]
    fn fd_request_round_trip_uses_typed_oci_parameters() {
        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::OciPull(Box::new(SpawnOciPullParams {
                cmd_id: 9,
                request: OciPullRequest {
                    reference: crate::nspawn::models::OciReference::new(
                        "docker.io/library/nginx:latest",
                    )
                    .unwrap(),
                    machine: MachineName::new("web-app").unwrap(),
                    read_only: true,
                },
            })),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "spawn_oci_pull");
        assert_eq!(json["params"]["cmd_id"], 9);
        assert_eq!(
            json["params"]["request"]["reference"],
            "docker.io/library/nginx:latest"
        );
        assert_eq!(json["params"]["request"]["machine"], "web-app");
        assert_eq!(json["params"]["request"]["read_only"], true);
        assert!(json["params"].get("program").is_none());
        assert!(json["params"].get("args").is_none());

        let parsed: FdRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed.operation, FdOperation::OciPull(_)));
    }

    #[test]
    fn fd_request_round_trip_for_image_import_contains_no_source_path() {
        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::ImportRawImage(ImportRawImageParams {
                machine: MachineName::new("test-machine").unwrap(),
            }),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "import_raw_image");
        assert_eq!(json["params"]["machine"], "test-machine");
        assert!(json["params"].get("path").is_none());
        assert!(json["params"].get("source").is_none());

        let parsed: FdRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(
            parsed.operation,
            FdOperation::ImportRawImage(ImportRawImageParams { .. })
        ));

        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::ImportTarImage(ImportTarRequest {
                target: crate::nspawn::adapters::rootfs::RootfsTarget::Machine {
                    machine: MachineName::new("test-machine").unwrap(),
                },
                source_origin:
                    crate::nspawn::ops::provision::image_operation::TarSourceOrigin::Remote,
                allow_unsafe_remote: true,
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "import_tar_image");
        assert_eq!(json["params"]["target"]["kind"], "machine");
        assert_eq!(json["params"]["target"]["machine"], "test-machine");
        assert_eq!(json["params"]["source_origin"], "remote");
        assert_eq!(json["params"]["allow_unsafe_remote"], true);
        assert!(json["params"].get("path").is_none());
        assert!(json["params"].get("source").is_none());
    }

    #[test]
    fn image_import_response_round_trip_preserves_warnings() {
        let response = image_import_response(Ok(ImageImportReport {
            warnings: vec!["upgrade tar before importing untrusted archives".into()],
        }));
        let json = serde_json::to_string(&response).unwrap();
        let response: ImportImageResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(
            response.warnings,
            ["upgrade tar before importing untrusted archives"]
        );
        assert!(response.error.is_none());
    }

    #[test]
    fn image_import_response_defaults_missing_warnings_to_empty() {
        let response: ImportImageResponse = serde_json::from_str(r#"{"error":null}"#).unwrap();
        assert!(response.warnings.is_empty());
        assert!(response.error.is_none());
    }

    #[test]
    fn fd_request_rejects_invalid_machine_name_and_terminal_size() {
        let invalid_name = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_journalctl","params":{{"name":"../escape"}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&invalid_name).is_err());

        let zero_size = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_terminal","params":{{"name":"test","size":{{"cols":80,"rows":0}}}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&zero_size).is_err());

        let out_of_range = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_terminal","params":{{"name":"test","size":{{"cols":65536,"rows":24}}}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&out_of_range).is_err());

        let unknown_parameter = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_journalctl","params":{{"name":"test","unexpected":true}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&unknown_parameter).is_err());
    }

    #[test]
    fn rpc_machine_name_validation_runs_on_daemon_request() {
        let valid = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "dbus_system_operation".into(),
            params: serde_json::json!({"name": "valid-machine"}),
        };
        assert_eq!(
            request_machine_name(&valid).unwrap().as_str(),
            "valid-machine"
        );

        let invalid = RpcRequest {
            params: serde_json::json!({"name": "../escape"}),
            ..valid
        };
        assert!(request_machine_name(&invalid).is_err());
    }

    #[tokio::test]
    async fn tar_runtime_assessment_rpc_returns_typed_result_and_rejects_parameters() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(1);
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 7,
            method: "assess_tar_runtime".into(),
            params: serde_json::json!({}),
        };

        assert!(matches!(
            handle_request(
                &request,
                &None::<crate::nspawn::adapters::comm::dbus::DbusBackend>,
                &out_tx,
                uzers::get_current_uid(),
            )
            .await,
            HandleOutcome::Spawned
        ));
        let response: RpcResponse = serde_json::from_str(&out_rx.recv().await.unwrap()).unwrap();
        assert!(response.error.is_none());
        let _: TarRuntimeAssessment = serde_json::from_value(response.result.unwrap()).unwrap();

        let invalid = RpcRequest {
            params: serde_json::json!({"program": "tar"}),
            ..request
        };
        assert!(matches!(
            handle_request(
                &invalid,
                &None::<crate::nspawn::adapters::comm::dbus::DbusBackend>,
                &out_tx,
                uzers::get_current_uid(),
            )
            .await,
            HandleOutcome::Sync(Err(error)) if error.contains("does not accept parameters")
        ));
    }

    #[tokio::test]
    async fn peer_credentials_identify_the_connecting_process() {
        let (client, _server) = UnixStream::pair().unwrap();
        let credentials = get_peer_credentials(&client).unwrap();

        assert_eq!(credentials.pid, std::process::id());
        assert_eq!(credentials.uid, uzers::get_current_uid());
        assert_eq!(authorize_fd_peer(credentials, credentials), Ok(()));
    }

    #[test]
    fn daemon_log_directory_is_private_and_owned() {
        use std::os::unix::fs::MetadataExt;

        let parent = tempfile::tempdir().unwrap();
        let log_dir = parent.path().join("logs");
        let uid = uzers::get_current_uid();
        configure_daemon_log_directory(&log_dir, uid).unwrap();
        let metadata = std::fs::symlink_metadata(&log_dir).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.mode() & 0o777, 0o700);
    }

    #[test]
    fn daemon_log_directory_rejects_a_symlink() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        let link = parent.path().join("logs");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = configure_daemon_log_directory(&link, uzers::get_current_uid()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn daemon_session_log_name_is_scoped_and_unambiguous() {
        let name = daemon_log_file_name();

        assert!(daemon_log_file_matches(Path::new(&name)));
        assert!(!daemon_log_file_matches(Path::new("daemon-unrelated.log")));
        assert!(!daemon_log_file_matches(Path::new(
            "daemon-20260817T120000Z-p1-snot-a-session.log"
        )));
    }

    #[test]
    fn daemon_session_log_stops_at_its_byte_limit() {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = tempfile::tempfile().unwrap();
        let mut writer = SessionLogWriter::with_limit(file.try_clone().unwrap(), 64);
        writer.write_all(&vec![b'x'; 256]).unwrap();
        writer.flush().unwrap();

        assert_eq!(file.metadata().unwrap().len(), 64);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert!(content.ends_with("[daemon log truncated at the per-session limit]\n"));
    }

    #[test]
    fn daemon_log_cleanup_retains_only_the_session_budget() {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let current = directory
            .path()
            .join("daemon-20260817T120000Z-p1-s00000000000000000000000000000000.log");
        for index in 0..=DAEMON_LOG_MAX_SESSIONS {
            let path = if index == 0 {
                current.clone()
            } else {
                directory.path().join(format!(
                    "daemon-20260817T120000Z-p{}-s{:032x}.log",
                    index + 1,
                    index
                ))
            };
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)
                .unwrap();
        }

        cleanup_daemon_logs(directory.path(), &current, uzers::get_current_uid()).unwrap();

        let retained = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| daemon_log_file_matches(&entry.path()))
            .count();
        assert_eq!(retained, DAEMON_LOG_MAX_SESSIONS);
        assert!(current.exists());
    }

    #[tokio::test]
    async fn pidfd_becomes_readable_after_process_exit() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pidfd = match open_pidfd(child.id()) {
            Ok(pidfd) => pidfd,
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(error) => panic!("pidfd_open failed: {error}"),
        };
        let async_pidfd = tokio::io::unix::AsyncFd::new(pidfd).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        let _ready =
            tokio::time::timeout(std::time::Duration::from_secs(1), async_pidfd.readable())
                .await
                .expect("pidfd did not become readable")
                .unwrap();
    }

    #[test]
    fn fd_socket_is_user_owned_and_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("fd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let uid = uzers::get_current_uid();

        configure_user_socket(&socket_path, uid).unwrap();

        let metadata = std::fs::symlink_metadata(&socket_path).unwrap();
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn fd_socket_directory_is_user_owned_and_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let uid = uzers::get_current_uid();
        let directory = create_fd_socket_dir(uid).unwrap();
        let metadata = std::fs::symlink_metadata(directory.path()).unwrap();

        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn socket_directory_falls_back_from_unusable_runtime_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let uid = uzers::get_current_uid();
        let parent = tempfile::tempdir().unwrap();
        let runtime = parent.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o500)).unwrap();

        assert!(!is_private_writable_runtime_dir(&runtime, uid));
        let directory =
            create_fd_socket_dir_from_candidates(uid, std::slice::from_ref(&runtime)).unwrap();
        assert!(!directory.path().starts_with(&runtime));
    }

    #[test]
    fn read_only_runtime_errors_are_eligible_for_fallback() {
        let error = std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem);
        assert!(runtime_dir_error_allows_fallback(&error));
    }
}
