//! Elevated daemon — a long-running root child process that executes
//! privileged commands and file I/O on behalf of the unprivileged TUI.
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
use crate::nspawn::sys::command::{CommandRunner, SpawnedProcess};
use fs2::FileExt;
use sendfd::{RecvWithFd, SendWithFd};
use serde::{Deserialize, Serialize};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

// ── RPC message types ──

const DAEMON_BOOTSTRAP_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct DaemonBootstrap {
    protocol_version: u32,
    fd_auth_token: String,
}

#[derive(Serialize, Deserialize)]
struct FdRequest {
    method: String,
    auth_token: String,
    #[serde(default)]
    params: serde_json::Value,
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

#[derive(Debug, Serialize, Deserialize)]
struct CommandResult {
    status: i32,
    stdout: String,
    stderr: String,
}

// ── DaemonCommandRunner — adapts ElevatedDaemon to CommandRunner ──

/// Routes commands through the elevated daemon (root child process).
pub struct DaemonCommandRunner {
    daemon: Arc<ElevatedDaemon>,
}

impl DaemonCommandRunner {
    pub fn new(daemon: Arc<ElevatedDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait::async_trait]
impl CommandRunner for DaemonCommandRunner {
    async fn run(&self, program: &str, args: Vec<String>) -> std::io::Result<Output> {
        self.daemon.run_command(program, &args).await
    }

    async fn spawn(&self, program: &str, args: Vec<String>) -> std::io::Result<SpawnedProcess> {
        let cmd_id = self.daemon.reserve_spawn_id();
        let stdout_fd = self.daemon.spawn_shell_cmd(cmd_id, program, &args).await?;
        let receiver = pipe_reader(stdout_fd)?;
        Ok(SpawnedProcess::new(Box::new(receiver), {
            let daemon = self.daemon.clone();
            async move {
                let code = daemon.wait_command(cmd_id).await?;
                Ok(ExitStatus::from_raw(code))
            }
        }))
    }
}

fn pipe_reader(fd: RawFd) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
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
    pub async fn spawn() -> std::io::Result<Self> {
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
        log::info!("[lasper] rpc_call: sending {} (id={})...", method, id);
        self.request_tx
            .send((request, response_tx))
            .await
            .map_err(|_| {
                log::info!("[lasper] rpc_call: mpsc send failed (I/O task stopped)");
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "daemon I/O task has stopped",
                )
            })?;
        log::info!("[lasper] rpc_call: waiting for response...");
        let response = response_rx.await.map_err(|_| {
            log::info!("[lasper] rpc_call: response channel cancelled");
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

    pub async fn run_command(&self, program: &str, args: &[String]) -> std::io::Result<Output> {
        let result = self
            .rpc_call(
                "run_command",
                serde_json::json!({"program": program, "args": args}),
            )
            .await?;
        let cmd_result: CommandResult = serde_json::from_value(result)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Output {
            status: std::process::ExitStatus::from_raw(cmd_result.status),
            stdout: cmd_result.stdout.into_bytes(),
            stderr: cmd_result.stderr.into_bytes(),
        })
    }

    pub async fn read_file(&self, path: &std::path::Path) -> crate::nspawn::errors::Result<String> {
        let result = self
            .rpc_call(
                "read_file",
                serde_json::json!({"path": path.to_string_lossy()}),
            )
            .await
            .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.to_path_buf(), e))?;
        let content = result["content"].as_str().ok_or_else(|| {
            crate::nspawn::errors::NspawnError::Runtime(
                "daemon read_file: missing content field".into(),
            )
        })?;
        Ok(content.to_string())
    }

    pub async fn write_file(
        &self,
        path: &std::path::Path,
        content: &str,
    ) -> crate::nspawn::errors::Result<()> {
        self.rpc_call(
            "write_file",
            serde_json::json!({"path": path.to_string_lossy(), "content": content}),
        )
        .await
        .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.to_path_buf(), e))?;
        Ok(())
    }

    pub async fn remove_file(&self, path: &std::path::Path) -> crate::nspawn::errors::Result<()> {
        self.rpc_call(
            "remove_file",
            serde_json::json!({"path": path.to_string_lossy()}),
        )
        .await
        .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.to_path_buf(), e))?;
        Ok(())
    }

    pub async fn create_dir_all(
        &self,
        path: &std::path::Path,
    ) -> crate::nspawn::errors::Result<()> {
        self.rpc_call(
            "create_dir_all",
            serde_json::json!({"path": path.to_string_lossy()}),
        )
        .await
        .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.to_path_buf(), e))?;
        Ok(())
    }

    pub async fn remove_dir_all(
        &self,
        path: &std::path::Path,
    ) -> crate::nspawn::errors::Result<()> {
        self.rpc_call(
            "remove_dir_all",
            serde_json::json!({"path": path.to_string_lossy()}),
        )
        .await
        .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.to_path_buf(), e))?;
        Ok(())
    }

    // ── FD-passing ──
    //
    // Connect and write the request async, then hand the fd to a blocking
    // thread for recv_with_fd (sendfd on tokio::UnixStream uses try_io which
    // is unreliable when mixed with prior read/write ops on the same fd).

    pub async fn spawn_journalctl(&self, name: &str) -> std::io::Result<RawFd> {
        let std_sock = self
            .open_fd_channel("spawn_journalctl", serde_json::json!({"name": name}))
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
        let std_sock = self
            .open_fd_channel(
                "spawn_login",
                serde_json::json!({"name": name, "cols": cols, "rows": rows}),
            )
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

    // ── Generic spawn (bootstrap / image pull / …) ──

    /// Allocate a unique ID for a spawned command.
    pub fn reserve_spawn_id(&self) -> u64 {
        self.next_spawn_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Spawn `sh -c 'exec "$@" 2>&1' -- program args…` as root, return
    /// the stdout fd.
    pub async fn spawn_shell_cmd(
        &self,
        cmd_id: u64,
        program: &str,
        args: &[String],
    ) -> std::io::Result<RawFd> {
        let std_sock = self
            .open_fd_channel(
                "spawn_shell_cmd",
                serde_json::json!({
                    "cmd_id": cmd_id,
                    "program": program,
                    "args": args,
                }),
            )
            .await?;
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 128];
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
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        use tokio::io::AsyncWriteExt;

        let mut sock = tokio::net::UnixStream::connect(&self.fd_sock_path).await?;
        let request = FdRequest {
            method: method.to_string(),
            auth_token: self.fd_auth_token.to_string(),
            params,
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

    let dbus: Option<crate::nspawn::adapters::comm::dbus::DbusBackend> =
        if crate::nspawn::adapters::comm::dbus::DbusBackend::new()
            .is_available()
            .await
        {
            Some(crate::nspawn::adapters::comm::dbus::DbusBackend::new())
        } else {
            None
        };

    if let Some(ref dbus) = dbus {
        let out_tx_bg = out_tx.clone();
        let dbus_bg = dbus.clone();
        tokio::spawn(async move {
            let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<()>(16);
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

        match handle_request(&request, &dbus, &out_tx).await {
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
) -> HandleOutcome {
    use crate::nspawn::adapters::comm::backend::ContainerBackend;

    match request.method.as_str() {
        "ping" => HandleOutcome::Sync(Ok(serde_json::Value::Null)),

        "run_command" => {
            let id = request.id;
            let program = match request.params["program"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing program".into())),
            };
            let args: Vec<String> = match request.params["args"].as_array() {
                Some(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                None => return HandleOutcome::Sync(Err("missing args".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let output = crate::nspawn::sys::new_command(&program)
                    .args(&args)
                    .output()
                    .await;
                let response = match output {
                    Ok(out) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{
                        "status": out.status.into_raw(),
                        "stdout": String::from_utf8_lossy(&out.stdout),
                        "stderr": String::from_utf8_lossy(&out.stderr),
                    }}),
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":format!("{}",e)}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "read_file" => {
            let id = request.id;
            let path_str = match request.params["path"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing path".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::fs::read_to_string(&path_str)
                    .await
                    .map_err(|e| format!("read {}: {}", path_str, e));
                let response = match result {
                    Ok(content) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"content":content}})
                    }
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":e}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "write_file" => {
            let id = request.id;
            let path_str = match request.params["path"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing path".into())),
            };
            let content = match request.params["content"].as_str() {
                Some(c) => c.to_owned(),
                None => return HandleOutcome::Sync(Err("missing content".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result: Result<(), String> = write_locked_impl(&path_str, &content).await;
                let response = match result {
                    Ok(()) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":e}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "remove_file" => {
            let id = request.id;
            let path_str = match request.params["path"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing path".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::fs::remove_file(&path_str)
                    .await
                    .map_err(|e| format!("rm {}: {}", path_str, e));
                let response = match result {
                    Ok(()) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":e}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "create_dir_all" => {
            let id = request.id;
            let path_str = match request.params["path"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing path".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::fs::create_dir_all(&path_str)
                    .await
                    .map_err(|e| format!("mkdir -p {}: {}", path_str, e));
                let response = match result {
                    Ok(()) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":e}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "remove_dir_all" => {
            let id = request.id;
            let path_str = match request.params["path"].as_str() {
                Some(p) => p.to_owned(),
                None => return HandleOutcome::Sync(Err("missing path".into())),
            };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::fs::remove_dir_all(&path_str)
                    .await
                    .map_err(|e| format!("rm -rf {}: {}", path_str, e));
                let response = match result {
                    Ok(()) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":null}),
                    Err(e) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":e}})
                    }
                };
                let line = serde_json::to_string(&response).unwrap();
                let _ = out_tx.send(line).await;
            });
            HandleOutcome::Spawned
        }

        "dbus_list_all" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_all().await {
                Ok(entries) => match serde_json::to_value(entries) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_start" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.start(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_terminate" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.terminate(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_poweroff" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.poweroff(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_reboot" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.reboot(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_enable" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.enable(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_disable" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.disable(&name).await.map_err(|e| e.to_string())
            })
            .await
        }
        "dbus_remove" => {
            sync_dbus_op(dbus, request, |dbus, name| async move {
                dbus.remove(&name).await.map_err(|e| e.to_string())
            })
            .await
        }

        "dbus_kill" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let name = match request.params["name"].as_str() {
                Some(n) => n,
                None => return HandleOutcome::Sync(Err("missing name".into())),
            };
            let signal = match request.params["signal"].as_str() {
                Some(s) => s,
                None => return HandleOutcome::Sync(Err("missing signal".into())),
            };
            match dbus.kill(name, signal).await {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_get_properties" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let name = match request.params["name"].as_str() {
                Some(n) => n,
                None => return HandleOutcome::Sync(Err("missing name".into())),
            };
            match dbus.get_properties(name).await {
                Ok(props) => match serde_json::to_value(props) {
                    Ok(v) => HandleOutcome::Sync(Ok(v)),
                    Err(e) => HandleOutcome::Sync(Err(e.to_string())),
                },
                Err(e) => HandleOutcome::Sync(Err(e.to_string())),
            }
        }

        "dbus_reload_daemon" => {
            let dbus = match dbus.as_ref() {
                Some(d) => d,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.reload_daemon().await {
                Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
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

async fn sync_dbus_op<F, Fut>(
    dbus: &Option<crate::nspawn::adapters::comm::dbus::DbusBackend>,
    request: &RpcRequest,
    f: F,
) -> HandleOutcome
where
    F: FnOnce(crate::nspawn::adapters::comm::dbus::DbusBackend, String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let dbus = match dbus.as_ref() {
        Some(d) => d,
        None => return HandleOutcome::Sync(Err("DBus not available".into())),
    };
    let name = match request.params["name"].as_str() {
        Some(n) => n.to_owned(),
        None => return HandleOutcome::Sync(Err("missing name".into())),
    };
    match f(dbus.clone(), name).await {
        Ok(()) => HandleOutcome::Sync(Ok(serde_json::Value::Null)),
        Err(e) => HandleOutcome::Sync(Err(e)),
    }
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

    let stream = buf_reader.into_inner();
    let method = request.method.as_str();

    // Convert to std stream for blocking send_with_fd.
    // tokio streams are non-blocking — must switch back so sendmsg
    // doesn't return EAGAIN.
    let std_stream = match stream.into_std() {
        Ok(s) => {
            let _ = s.set_nonblocking(false);
            s
        }
        Err(e) => {
            log::error!("Daemon fd-handler: into_std failed: {}", e);
            return;
        }
    };

    match method {
        "spawn_journalctl" => {
            let name = request.params["name"].as_str().unwrap_or("");
            match crate::nspawn::sys::new_sync_command("journalctl")
                .args([
                    "-M",
                    name,
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

        "spawn_login" => {
            let name = request.params["name"].as_str().unwrap_or("");
            let cols: u16 = request.params["cols"].as_u64().unwrap_or(80) as u16;
            let rows: u16 = request.params["rows"].as_u64().unwrap_or(24) as u16;

            use portable_pty::{native_pty_system, CommandBuilder, PtySize};
            let pty_system = native_pty_system();
            let pair = match pty_system.openpty(PtySize {
                rows,
                cols,
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
            cmd.args(["login", name]);
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

        "spawn_shell_cmd" => {
            let cmd_id = request.params["cmd_id"].as_u64().unwrap_or(0);
            let program = request.params["program"].as_str().unwrap_or("").to_string();
            let args: Vec<String> = request.params["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

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

                    if let Err(e) = std_stream.send_with_fd(b"ok", &[raw_fd]) {
                        log::error!("Daemon: send_with_fd (spawn_shell_cmd) failed: {}", e);
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
                    log::error!("Daemon: spawn shell cmd failed: {}", e);
                    let _ = std_stream.send_with_fd(b"spawn failed", &[]);
                }
            }
        }

        _ => log::warn!("Daemon fd-handler: unknown method: {}", method),
    }
}

static SPAWN_EXIT_CODES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, i32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

// ── Atomic locked write (daemon side) ──

/// Lock → write tmp → fsync → rename → parent fsync → unlock.
///
/// Mirrors [`crate::nspawn::sys::io::AsyncLockedWriter::write_locked`]
/// but runs inside the root daemon so all file I/O is privileged.
async fn write_locked_impl(path_str: &str, content: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::time::{sleep, Duration};

    let path = std::path::Path::new(path_str);
    let lock_path = path.with_extension("lock");
    let tmp_path = path.with_extension("tmp");

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir -p {}: {}", parent.display(), e))?;
    }

    // Acquire exclusive lock (async backoff loop)
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("open lock {:?}: {}", lock_path, e))?;

    let mut attempts = 0;
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(_) => break,
            Err(_) if attempts < 100 => {
                attempts += 1;
                sleep(Duration::from_millis(10)).await;
            }
            Err(e) => {
                return Err(format!(
                    "could not acquire lock on {:?} after {} attempts: {}",
                    lock_path, attempts, e
                ));
            }
        }
    }

    // Write + fsync to temp file
    {
        let mut f = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| format!("create tmp {:?}: {}", tmp_path, e))?;
        f.write_all(content.as_bytes())
            .await
            .map_err(|e| format!("write tmp {:?}: {}", tmp_path, e))?;
        f.sync_data()
            .await
            .map_err(|e| format!("fsync tmp {:?}: {}", tmp_path, e))?;
    }

    // Atomic rename
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| format!("rename {:?} -> {:?}: {}", tmp_path, path, e))?;

    // Sync parent directory
    if let Some(parent) = path.parent() {
        if let Ok(dir) = tokio::fs::File::open(parent).await {
            let _ = dir.sync_all().await;
        }
    }

    // Remove lock file before closing handle (safe on Linux: unlink while open)
    let _ = tokio::fs::remove_file(&lock_path).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "f865fd7e-a9f5-4ef1-b5b5-f3f257a75ce0";

    #[test]
    fn test_command_result_serde() {
        let cr = CommandResult {
            status: 0,
            stdout: "hello".into(),
            stderr: String::new(),
        };
        let json = serde_json::to_value(&cr).unwrap();
        assert_eq!(json["status"], 0);
        assert_eq!(json["stdout"], "hello");
        let parsed: CommandResult = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.status, 0);
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
        let request = r#"{"method":"spawn_shell_cmd","params":{"program":"id","args":[]}}"#;
        assert!(serde_json::from_str::<FdRequest>(request).is_err());
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
