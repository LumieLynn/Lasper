//! Authenticated daemon client facade, RPC multiplexer, and typed proxies.

mod session;

use crate::adapters::config::store::{NspawnConfigOperation, NspawnConfigResult};
use crate::adapters::config::systemd_unit::{SystemdUnitOperation, SystemdUnitResult};
use crate::adapters::platform::nvidia::state::{NvidiaStateOperation, NvidiaStateResult};
use crate::adapters::provisioning::engine::bootstrap_operation::BootstrapRequest;
use crate::adapters::provisioning::engine::image_operation::{
    ImageImportReport, ImportTarRequest, TarRuntimeAssessment,
};
use crate::adapters::provisioning::engine::oci_operation::OciPullRequest;
use crate::adapters::rootfs::store::{RootfsOperation, RootfsResult};
use crate::adapters::storage::store::{ManagedStorageOperation, ManagedStorageResult};
use crate::adapters::system_operation::SystemOperation;
use crate::application::image_lifecycle::{ImageControlOutcome, ImageRemoveRequest};
use crate::application::machine_lifecycle::{MachineControlOutcome, MachineControlRequest};
use crate::daemon::protocol::*;
use crate::daemon::transport::{
    authorize_root_server, connect_rpc_socket, create_fd_socket_dir, get_peer_credentials,
    read_bounded_line, MAX_RPC_FRAME_BYTES,
};
use crate::domain::secret::SecretBytes;
use crate::nspawn::models::{MachineName, MachineProperties};
use sendfd::{RecvWithFd, SendWithFd};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn pipe_reader(
    fd: std::os::fd::RawFd,
) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    tokio::net::unix::pipe::Receiver::from_owned_fd(owned)
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
        let mut rpc_bootstrap_line =
            SecretBytes::new(serde_json::to_vec(&rpc_bootstrap).map_err(|error| {
                std::io::Error::other(format!("serialize RPC authentication: {error}"))
            })?);
        rpc_bootstrap_line.push(b'\n');
        rpc_stream.write_all(rpc_bootstrap_line.as_slice()).await?;
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

                        let id = request.id();

                        let mut req_line = match request.into_wire_bytes() {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                log::error!("Daemon I/O: failed to serialize request: {}", e);
                                continue;
                            }
                        };
                        req_line.push(b'\n');
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

                        if let Err(e) = writer.write_all(req_line.as_slice()).await {
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
        self.send_rpc_request(OutboundRpcRequest::General(request))
            .await
    }

    async fn send_rpc_request(
        &self,
        request: OutboundRpcRequest,
    ) -> std::io::Result<serde_json::Value> {
        let id = request.id();
        let method = request.method();
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

    pub(crate) async fn image_remove(
        &self,
        request: ImageRemoveRequest,
    ) -> std::io::Result<ImageControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("image_remove", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn machine_control(
        &self,
        request: MachineControlRequest,
    ) -> std::io::Result<MachineControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("machine_control", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = self
            .send_rpc_request(OutboundRpcRequest::Rootfs { id, operation })
            .await?;
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

    pub(super) async fn open_fd_channel(
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
