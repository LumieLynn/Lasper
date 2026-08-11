//! Elevated daemon — a long-running root child process that executes closed,
//! typed privileged operations on behalf of the unprivileged TUI.
//!
//! Communication is JSON-RPC 2.0 over stdin/stdout (one JSON object per
//! line). The daemon is spawned via `sudo <self> --daemon` before the
//! terminal enters raw mode, so the sudo password prompt appears on the
//! clean terminal.
//!
//! ## Architecture
//!
//! A dedicated I/O task serializes all RPC traffic through the child's
//! stdin/stdout pipes. Callers send `(request, oneshot::Sender)` tuples
//! over an mpsc channel; the I/O task writes the request, reads the
//! response, and fulfills the oneshot. This avoids locking issues around
//! async pipe I/O.
//!
//! ## FD passing
//!
//! Long-running commands (journalctl -f, machinectl login) are spawned by
//! the daemon as root. The daemon passes the resulting fd back to the parent
//! over a Unix domain socket using the [`sendfd`] crate. The socket is scoped
//! to a private per-session directory, owned by the launching user, and each
//! connection must match both the TUI's PID/UID via `SO_PEERCRED` and a random
//! session token delivered through the private stdin bootstrap pipe.

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
use crate::nspawn::models::{MachineName, MachineProperties, TerminalSize};
use crate::nspawn::ops::provision::bootstrap_operation::{
    build_command as build_bootstrap_command, probe_debootstrap_signature_style_sync,
    validate_target as validate_bootstrap_target, BootstrapRequest,
};
use crate::nspawn::ops::provision::image_operation::ImportTarRequest;
use crate::nspawn::ops::provision::oci_operation::{
    build_command as build_oci_pull_command, OciPullRequest,
};
use crate::nspawn::ops::system_operation::{
    execute_dbus_system_operation, execute_system_operation, SystemOperation,
};
use crate::nspawn::platform::nvidia::state::{
    execute_nvidia_state_operation, NvidiaStateOperation, NvidiaStateResult,
};
use sendfd::{RecvWithFd, SendWithFd};
use serde::{Deserialize, Serialize};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

// ── RPC message types ──

const DAEMON_BOOTSTRAP_VERSION: u32 = 6;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonBootstrap {
    protocol_version: u32,
    fd_auth_token: String,
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
    #[serde(rename = "spawn_login")]
    Login(SpawnLoginParams),
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
struct SpawnLoginParams {
    name: MachineName,
    size: TerminalSize,
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
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
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
    dbus.is_available().await.then_some(dbus)
}

// ── Parent-side handle ──

pub struct ElevatedDaemon {
    request_tx: tokio::sync::mpsc::Sender<RpcCall>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    event_tx: tokio::sync::broadcast::Sender<()>,
    pid: u32,
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let runtime_dir = xdg_runtime_dir
        .filter(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.is_dir() && metadata.uid() == user_uid && metadata.mode() & 0o077 == 0
            })
        })
        .or_else(|| {
            let path = PathBuf::from(format!("/run/user/{}", user_uid));
            std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| {
                    metadata.is_dir() && metadata.uid() == user_uid && metadata.mode() & 0o077 == 0
                })
                .then_some(path)
        });

    let mut builder = tempfile::Builder::new();
    builder.prefix("lasper-");
    builder.permissions(std::fs::Permissions::from_mode(0o700));
    let directory = match runtime_dir {
        Some(path) => builder.tempdir_in(path),
        None => builder.tempdir(),
    }?;
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

fn configure_fd_socket(path: &Path, user_uid: u32) -> std::io::Result<()> {
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
            "fd socket ownership or mode verification failed",
        ));
    }
    Ok(())
}

impl ElevatedDaemon {
    pub async fn spawn(dbus_enabled: bool) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let user_uid = uzers::get_current_uid();
        let parent_pid = std::process::id();
        let fd_auth_token: Arc<str> = Arc::from(uuid::Uuid::new_v4().to_string());
        let fd_sock_dir = Arc::new(create_fd_socket_dir(user_uid)?);
        let sock_path = fd_sock_dir.path().join("fd.sock");

        let mut child = tokio::process::Command::new("sudo")
            .arg(&exe)
            .arg("--daemon")
            .arg("--fd-sock")
            .arg(&sock_path)
            .arg("--daemon-uid")
            .arg(user_uid.to_string())
            .arg("--daemon-pid")
            .arg(parent_pid.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("failed to launch sudo daemon: {}", error),
                )
            })?;

        let pid = child.id().expect("child has pid");

        let child_stdin = child.stdin.take().expect("stdin piped");
        let child_stdout = child.stdout.take().expect("stdout piped");

        let (request_tx, mut request_rx) = tokio::sync::mpsc::channel::<RpcCall>(8);
        let (event_tx, _) = tokio::sync::broadcast::channel::<()>(16);

        let io_pid = pid;
        let event_tx_io = event_tx.clone();
        let bootstrap_token = fd_auth_token.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

            let mut stdin = tokio::io::BufWriter::new(child_stdin);
            let mut stdout = tokio::io::BufReader::new(child_stdout);
            let mut line_buf = String::new();

            let bootstrap = DaemonBootstrap {
                protocol_version: DAEMON_BOOTSTRAP_VERSION,
                fd_auth_token: bootstrap_token.to_string(),
                dbus_enabled,
            };
            let mut bootstrap_line = match serde_json::to_vec(&bootstrap) {
                Ok(line) => line,
                Err(e) => {
                    log::error!("Daemon I/O: failed to serialize bootstrap: {}", e);
                    return;
                }
            };
            bootstrap_line.push(b'\n');
            if let Err(e) = stdin.write_all(&bootstrap_line).await {
                log::error!("Daemon I/O: failed to write bootstrap: {}", e);
                return;
            }
            if let Err(e) = stdin.flush().await {
                log::error!("Daemon I/O: failed to flush bootstrap: {}", e);
                return;
            }

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

                        if let Err(e) = stdin.write_all(req_line.as_bytes()).await {
                            log::error!("Daemon I/O: failed to write to child stdin: {}", e);
                            break;
                        }
                        if let Err(e) = stdin.flush().await {
                            log::error!("Daemon I/O: failed to flush child stdin: {}", e);
                            break;
                        }

                        pending.insert(id, response_tx);
                    }

                    read_res = stdout.read_line(&mut line_buf) => {
                        match read_res {
                            Ok(0) => {
                                log::info!("[lasper] I/O task: daemon stdout EOF");
                                log::error!("Daemon I/O: child stdout closed (EOF)");
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                log::error!("Daemon I/O: failed to read child stdout: {}", e);
                                break;
                            }
                        }

                        let raw: serde_json::Value = match serde_json::from_str(&line_buf) {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!("Daemon I/O: failed to parse JSON: {}", e);
                                line_buf.clear();
                                continue;
                            }
                        };
                        line_buf.clear();

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
                }
            }

            log::info!(
                "[lasper] I/O task exiting, waiting for child pid={}...",
                io_pid
            );
            drop(stdin);
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
            fd_sock_path: sock_path,
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

    pub async fn spawn_login(&self, name: &str, cols: u16, rows: u16) -> std::io::Result<RawFd> {
        let name = MachineName::try_from(name)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let size = TerminalSize::new(cols, rows)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let std_sock = self
            .open_fd_channel(FdOperation::Login(SpawnLoginParams { name, size }))
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
    }

    pub(crate) async fn import_tar_image(
        &self,
        request: ImportTarRequest,
        source: std::fs::File,
    ) -> std::io::Result<()> {
        self.import_image_source(FdOperation::ImportTarImage(request), source)
            .await
    }

    async fn import_image_source(
        &self,
        operation: FdOperation,
        source: std::fs::File,
    ) -> std::io::Result<()> {
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
                None => Ok(()),
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
}

// ── Daemon main loop (child side) ──

pub async fn daemon_main(
    fd_sock_opt: Option<PathBuf>,
    user_uid: u32,
    expected_parent_pid: u32,
) -> ! {
    use tokio::io::AsyncBufReadExt;

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    let bootstrap_line = match lines.next_line().await {
        Ok(Some(line)) => line,
        Ok(None) => {
            eprintln!("lasper daemon: missing bootstrap message");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("lasper daemon: failed to read bootstrap: {}", e);
            std::process::exit(1);
        }
    };
    let bootstrap: DaemonBootstrap = match serde_json::from_str(&bootstrap_line) {
        Ok(bootstrap) => bootstrap,
        Err(e) => {
            eprintln!("lasper daemon: invalid bootstrap message: {}", e);
            std::process::exit(1);
        }
    };
    if bootstrap.protocol_version != DAEMON_BOOTSTRAP_VERSION
        || uuid::Uuid::parse_str(&bootstrap.fd_auth_token).is_err()
        || expected_parent_pid == 0
    {
        eprintln!("lasper daemon: rejected bootstrap message");
        std::process::exit(1);
    }
    let fd_auth_token: Arc<str> = Arc::from(bootstrap.fd_auth_token);

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(32);

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
        std::process::exit(0);
    });

    let dbus = initialize_dbus_backend(bootstrap.dbus_enabled).await;

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
    if let Err(e) = configure_fd_socket(&sock_path, user_uid) {
        log::error!("Daemon: failed to secure fd socket: {}", e);
        std::process::exit(1);
    }

    let expected_peer = PeerCredentials {
        pid: expected_parent_pid,
        uid: user_uid,
    };
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle_fd_connection(
                        stream,
                        expected_peer,
                        fd_auth_token.clone(),
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
            std::process::exit(0);
        }
    }

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => std::process::exit(0),
            Err(e) => {
                log::error!("Daemon stdin error: {}", e);
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

async fn handle_request(
    request: &RpcRequest,
    dbus: &Option<crate::nspawn::adapters::comm::dbus::DbusBackend>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
    invoking_uid: u32,
) -> HandleOutcome {
    use crate::nspawn::adapters::comm::backend::ContainerBackend;

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
            match execute_dbus_system_operation(dbus, operation).await {
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

        "exit" => {
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
) {
    use tokio::io::AsyncBufReadExt;

    let actual_peer = match get_peer_credentials(&stream) {
        Ok(credentials) => credentials,
        Err(e) => {
            log::warn!("Daemon fd-handler: SO_PEERCRED failed: {}", e);
            return;
        }
    };
    if let Err(e) = authorize_fd_peer(actual_peer, expected_peer) {
        match e {
            FdAuthorizationError::UnexpectedUid { actual, expected } => {
                log::warn!(
                    "Daemon fd-handler: rejected uid {} (expected {})",
                    actual,
                    expected
                );
            }
            FdAuthorizationError::UnexpectedPid { actual, expected } => {
                log::warn!(
                    "Daemon fd-handler: rejected pid {} (expected {})",
                    actual,
                    expected
                );
            }
            FdAuthorizationError::InvalidToken => unreachable!(),
        }
        return;
    }

    let mut buf_reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    if buf_reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
        log::warn!("Daemon fd-handler: failed to read request line");
        return;
    }

    let request: FdRequest = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Daemon fd-handler: parse error: {}", e);
            return;
        }
    };
    if authorize_fd_token(&request.auth_token, &expected_auth_token).is_err() {
        log::warn!(
            "Daemon fd-handler: rejected unauthenticated request from pid {}",
            actual_peer.pid
        );
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
                .spawn()
            {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("stdout piped");
                    let raw_fd = stdout.as_raw_fd();

                    if let Err(e) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (journalctl) failed: {}", e);
                    }
                    drop(stdout);
                    tokio::task::spawn_blocking(move || {
                        let _ = child.wait();
                    });
                }
                Err(e) => {
                    log::error!("Daemon: spawn journalctl failed: {}", e);
                }
            }
        }

        FdOperation::Login(SpawnLoginParams { name, size }) => {
            use portable_pty::{native_pty_system, CommandBuilder, PtySize};
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
                    return;
                }
            };

            let mut cmd = CommandBuilder::new("machinectl");
            cmd.args(["login", name.as_str()]);
            match pair.slave.spawn_command(cmd) {
                Ok(mut child) => {
                    drop(pair.slave);
                    let master_fd = pair.master.as_raw_fd().expect("master has fd");

                    if let Err(e) = std_stream.send_with_fd(b"ok", &[master_fd]) {
                        log::error!("Daemon: send_with_fd (login) failed: {}", e);
                    }
                    drop(pair.master);
                    tokio::task::spawn_blocking(move || {
                        let _ = child.wait();
                    });
                }
                Err(e) => {
                    log::error!("Daemon: spawn login failed: {}", e);
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
            match crate::nspawn::sys::new_sync_command("sh")
                .arg("-c")
                .arg("exec \"$@\" 2>&1")
                .arg("--")
                .arg(&program)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("stdout piped");
                    let raw_fd = stdout.as_raw_fd();
                    if let Err(error) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (spawn_bootstrap) failed: {}", error);
                    }
                    drop(stdout);

                    tokio::task::spawn_blocking(move || {
                        let status = child.wait();
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
            let (program, args) = build_oci_pull_command(&request);
            log::info!(
                "[AUDIT] [Step: OCI] Starting typed importctl pull-oci operation for {}",
                request.machine
            );
            match crate::nspawn::sys::new_sync_command("sh")
                .arg("-c")
                .arg("exec \"$@\" 2>&1")
                .arg("--")
                .arg(program)
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("stdout piped");
                    let raw_fd = stdout.as_raw_fd();

                    if let Err(e) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (spawn_oci_pull) failed: {}", e);
                    }
                    drop(stdout);

                    tokio::task::spawn_blocking(move || {
                        let status = child.wait();
                        if let Ok(s) = status {
                            let raw = s.into_raw();
                            SPAWN_EXIT_CODES.lock().insert(cmd_id, raw);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Daemon: typed OCI pull spawn failed: {}", e);
                    let _ = std_stream.send_with_fd(b"OCI pull spawn failed", &[]);
                }
            }
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
    result: std::result::Result<(), String>,
) {
    use std::io::Write;

    let response = ImportImageResponse {
        error: result.err(),
    };
    if let Ok(line) = serde_json::to_string(&response) {
        let _ = stream.write_all(format!("{line}\n").as_bytes());
    }
}

static SPAWN_EXIT_CODES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, i32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "f865fd7e-a9f5-4ef1-b5b5-f3f257a75ce0";

    #[test]
    fn daemon_bootstrap_carries_the_selected_transport_mode() {
        let bootstrap = DaemonBootstrap {
            protocol_version: DAEMON_BOOTSTRAP_VERSION,
            fd_auth_token: TEST_TOKEN.to_string(),
            dbus_enabled: false,
        };

        let json = serde_json::to_value(&bootstrap).unwrap();
        assert_eq!(json["dbus_enabled"], false);

        let parsed: DaemonBootstrap = serde_json::from_value(json).unwrap();
        assert!(!parsed.dbus_enabled);
        assert_eq!(parsed.protocol_version, DAEMON_BOOTSTRAP_VERSION);
    }

    #[test]
    fn daemon_bootstrap_rejects_missing_or_unknown_transport_fields() {
        let missing_mode = serde_json::json!({
            "protocol_version": DAEMON_BOOTSTRAP_VERSION,
            "fd_auth_token": TEST_TOKEN,
        });
        assert!(serde_json::from_value::<DaemonBootstrap>(missing_mode).is_err());

        let unknown_field = serde_json::json!({
            "protocol_version": DAEMON_BOOTSTRAP_VERSION,
            "fd_auth_token": TEST_TOKEN,
            "dbus_enabled": false,
            "unexpected": true,
        });
        assert!(serde_json::from_value::<DaemonBootstrap>(unknown_field).is_err());
    }

    #[tokio::test]
    async fn cli_mode_skips_daemon_dbus_initialization() {
        assert!(initialize_dbus_backend(false).await.is_none());
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
    fn fd_request_round_trip_uses_typed_login_parameters() {
        let request = FdRequest {
            auth_token: TEST_TOKEN.to_string(),
            operation: FdOperation::Login(SpawnLoginParams {
                name: MachineName::new("test-machine").unwrap(),
                size: TerminalSize::new(120, 40).unwrap(),
            }),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "spawn_login");
        assert_eq!(json["params"]["name"], "test-machine");
        assert_eq!(json["params"]["size"]["cols"], 120);
        assert_eq!(json["params"]["size"]["rows"], 40);

        let parsed: FdRequest = serde_json::from_value(json).unwrap();
        match parsed.operation {
            FdOperation::Login(params) => {
                assert_eq!(params.name.as_str(), "test-machine");
                assert_eq!(params.size, TerminalSize::new(120, 40).unwrap());
            }
            _ => panic!("expected spawn_login"),
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
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "import_tar_image");
        assert_eq!(json["params"]["target"]["kind"], "machine");
        assert_eq!(json["params"]["target"]["machine"], "test-machine");
        assert!(json["params"].get("path").is_none());
        assert!(json["params"].get("source").is_none());
    }

    #[test]
    fn fd_request_rejects_invalid_machine_name_and_terminal_size() {
        let invalid_name = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_journalctl","params":{{"name":"../escape"}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&invalid_name).is_err());

        let zero_size = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_login","params":{{"name":"test","size":{{"cols":80,"rows":0}}}}}}"#
        );
        assert!(serde_json::from_str::<FdRequest>(&zero_size).is_err());

        let out_of_range = format!(
            r#"{{"auth_token":"{TEST_TOKEN}","method":"spawn_login","params":{{"name":"test","size":{{"cols":65536,"rows":24}}}}}}"#
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
    async fn peer_credentials_identify_the_connecting_process() {
        let (client, _server) = UnixStream::pair().unwrap();
        let credentials = get_peer_credentials(&client).unwrap();

        assert_eq!(credentials.pid, std::process::id());
        assert_eq!(credentials.uid, uzers::get_current_uid());
        assert_eq!(authorize_fd_peer(credentials, credentials), Ok(()));
    }

    #[test]
    fn fd_socket_is_user_owned_and_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("fd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let uid = uzers::get_current_uid();

        configure_fd_socket(&socket_path, uid).unwrap();

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
}
