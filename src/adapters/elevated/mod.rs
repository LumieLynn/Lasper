//! Authenticated daemon client facade, RPC multiplexer, and typed proxies.

mod session;

use crate::adapters::config::store::{NspawnConfigOperation, NspawnConfigResult};
use crate::adapters::config::systemd_unit::{SystemdUnitOperation, SystemdUnitResult};
use crate::adapters::platform::nvidia::state::{NvidiaStateOperation, NvidiaStateResult};
use crate::adapters::provisioning::engine::image_operation::TarRuntimeAssessment;
use crate::adapters::provisioning::state::{DeploymentStateOperation, DeploymentStateResult};
use crate::adapters::rootfs::store::{RootfsOperation, RootfsResult};
use crate::adapters::system_operation::SystemOperation;
use crate::application::image_lifecycle::{ImageControlOutcome, ImageRemoveRequest};
use crate::application::machine_lifecycle::{MachineControlOutcome, MachineControlRequest};
use crate::daemon::protocol::deployment::{
    DeploymentJobRequest, DeploymentJobSnapshot, DeploymentSubmissionRequest,
    DeploymentSubmissionSnapshot, ProbeDeploymentRecoveryRequest, ProbeDeploymentRecoveryResult,
    ReleaseUnresolvedDeploymentRequest, SubmitDeploymentParams,
};
use crate::daemon::protocol::*;
use crate::daemon::server::transport::{
    authorize_root_server, connect_rpc_socket, create_fd_socket_dir, get_peer_credentials,
    read_bounded_line, MAX_RPC_FRAME_BYTES,
};
use crate::domain::secret::SecretBytes;
use crate::nspawn::models::{MachineName, MachineProperties};
use sendfd::{RecvWithFd, SendWithFd};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

const RPC_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type PendingRpcResponses =
    std::collections::HashMap<u64, tokio::sync::oneshot::Sender<RpcResponse>>;

fn rpc_response_timeout(family: RpcFamily) -> std::time::Duration {
    match family {
        RpcFamily::Query | RpcFamily::Session => std::time::Duration::from_secs(15),
        RpcFamily::Job => std::time::Duration::from_secs(30),
        RpcFamily::Command => std::time::Duration::from_secs(60),
    }
}

fn rpc_timeout_error(method: RpcMethod, timeout: std::time::Duration) -> std::io::Error {
    let consequence = match method.family() {
        RpcFamily::Query => "the query can be retried",
        RpcFamily::Session => "the session request was not confirmed",
        RpcFamily::Job => "deployment state must be reconciled before retrying",
        RpcFamily::Command => "the operation may still be running and its outcome is unknown",
    };
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "daemon {} '{}' timed out after {}s; {}",
            method.family().as_str(),
            method.wire_name(),
            timeout.as_secs(),
            consequence
        ),
    )
}

async fn wait_for_rpc_response(
    response_rx: tokio::sync::oneshot::Receiver<RpcResponse>,
    method: RpcMethod,
    timeout: std::time::Duration,
) -> std::io::Result<RpcResponse> {
    match tokio::time::timeout(timeout, response_rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "daemon response channel cancelled",
        )),
        Err(_) => Err(rpc_timeout_error(method, timeout)),
    }
}

fn discard_cancelled_responses(pending: &mut PendingRpcResponses) -> usize {
    let before = pending.len();
    pending.retain(|_, response_tx| !response_tx.is_closed());
    before - pending.len()
}

pub(crate) fn pipe_reader(
    fd: std::os::fd::RawFd,
) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    tokio::net::unix::pipe::Receiver::from_owned_fd(owned)
}

fn configure_sudo_daemon_stdio(command: &mut tokio::process::Command) {
    // sudo-rs with use_pty relays the controlling terminal for as long as a
    // foreground command lives. A stdout pipe makes this a non-foreground I/O
    // path, so the long-lived daemon cannot compete with the TUI for input.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
}

// ── Parent-side handle ──

pub(crate) struct ElevatedDaemon {
    request_tx: tokio::sync::mpsc::Sender<RpcCall>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    event_tx: tokio::sync::broadcast::Sender<()>,
    pid: u32,
    rpc_sock_path: PathBuf,
    fd_sock_path: PathBuf,
    fd_auth_token: Arc<str>,
    _fd_sock_dir: Arc<tempfile::TempDir>,
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

impl ElevatedDaemon {
    pub(crate) async fn spawn(dbus_enabled: bool) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let user_uid = uzers::get_current_uid();
        let parent_pid = std::process::id();
        let fd_auth_token: Arc<str> = Arc::from(uuid::Uuid::new_v4().to_string());
        let fd_sock_dir = Arc::new(create_fd_socket_dir(user_uid)?);
        let fd_sock_path = fd_sock_dir.path().join("fd.sock");
        let rpc_sock_path = fd_sock_dir.path().join("rpc.sock");

        let mut command = tokio::process::Command::new("sudo");
        command
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
            .arg(parent_pid.to_string());
        configure_sudo_daemon_stdio(&mut command);
        let mut child = command.spawn().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to launch sudo daemon: {}", error),
            )
        })?;

        let pid = child.id().expect("child has pid");
        let mut sudo_stdout = child.stdout.take().expect("sudo stdout was piped");
        let sudo_stdout_task = tokio::spawn(async move {
            if let Err(error) = tokio::io::copy(&mut sudo_stdout, &mut tokio::io::sink()).await {
                log::debug!("Failed to drain elevated daemon stdout: {error}");
            }
        });

        use tokio::io::AsyncWriteExt;
        let mut rpc_stream = connect_rpc_socket(&rpc_sock_path).await?;
        authorize_root_server(get_peer_credentials(&rpc_stream)?)?;
        let rpc_bootstrap = RpcBootstrap {
            protocol_version: RPC_PROTOCOL_VERSION,
            auth_token: fd_auth_token.to_string(),
            dbus_enabled,
        };
        let rpc_bootstrap_line =
            SecretBytes::new(serde_json::to_vec(&rpc_bootstrap).map_err(|error| {
                std::io::Error::other(format!("serialize RPC authentication: {error}"))
            })?);
        rpc_stream.write_all(rpc_bootstrap_line.as_slice()).await?;
        rpc_stream.write_all(b"\n").await?;
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

            let mut pending = PendingRpcResponses::new();
            let mut pending_sweep = tokio::time::interval(std::time::Duration::from_secs(5));
            pending_sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = pending_sweep.tick() => {
                        let discarded = discard_cancelled_responses(&mut pending);
                        if discarded > 0 {
                            log::debug!(
                                "Daemon I/O: discarded {discarded} cancelled response slots"
                            );
                        }
                    }

                    req_opt = request_rx.recv() => {
                        let (request, response_tx) = match req_opt {
                            Some(r) => r,
                            None => break,
                        };

                        if response_tx.is_closed() {
                            continue;
                        }

                        let id = request.id();

                        let req_line = match request.into_wire_bytes() {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                log::error!("Daemon I/O: failed to serialize request: {}", e);
                                continue;
                            }
                        };
                        if req_line.len().saturating_add(1) > MAX_RPC_FRAME_BYTES {
                            let _ = response_tx.send(RpcResponse {
                                jsonrpc: "2.0".into(),
                                id,
                                result: None,
                                error: Some(RpcError {
                                    code: error_code::INTERNAL_ERROR,
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
                        if let Err(e) = writer.write_all(b"\n").await {
                            log::error!("Daemon I/O: failed to terminate RPC request: {}", e);
                            break;
                        }
                        if let Err(e) = writer.flush().await {
                            log::error!("Daemon I/O: failed to flush daemon RPC socket: {}", e);
                            break;
                        }

                        if !response_tx.is_closed() {
                            pending.insert(id, response_tx);
                        }
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
            let _ = sudo_stdout_task.await;
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

    pub(super) fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.event_tx.subscribe()
    }

    // ── JSON-RPC dispatch ──

    pub(super) async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<serde_json::Value> {
        let method_kind = RpcMethod::parse(method).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown daemon RPC method: {method}"),
            )
        })?;
        log::trace!(
            "[lasper] RPC method {} ({})",
            method_kind.wire_name(),
            method_kind.family().as_str()
        );
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };
        self.send_rpc_request(OutboundRpcRequest::General(request), method_kind)
            .await
    }

    async fn send_rpc_request(
        &self,
        request: OutboundRpcRequest,
        method_kind: RpcMethod,
    ) -> std::io::Result<serde_json::Value> {
        let id = request.id();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        log::trace!(
            "[lasper] rpc_call: sending {} (id={})",
            method_kind.wire_name(),
            id
        );
        match tokio::time::timeout(
            RPC_QUEUE_TIMEOUT,
            self.request_tx.send((request, response_tx)),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                log::warn!("[lasper] rpc_call: mpsc send failed (I/O task stopped)");
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "daemon I/O task has stopped",
                ));
            }
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "daemon request queue remained full for {}s; '{}' was not submitted",
                        RPC_QUEUE_TIMEOUT.as_secs(),
                        method_kind.wire_name()
                    ),
                ));
            }
        }
        log::trace!("[lasper] rpc_call: waiting for response id={}", id);
        let timeout = rpc_response_timeout(method_kind.family());
        let response = wait_for_rpc_response(response_rx, method_kind, timeout)
            .await
            .inspect_err(|error| {
                log::warn!(
                    "[lasper] rpc_call: response failed id={} method={}: {}",
                    id,
                    method_kind.wire_name(),
                    error
                );
            })?;
        if let Some(err) = response.error {
            return Err(std::io::Error::other(format!(
                "daemon error (code={}): {}",
                err.code, err.message
            )));
        }
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    }

    async fn ping(&self) -> std::io::Result<()> {
        self.rpc_call("ping", serde_json::json!({})).await?;
        Ok(())
    }

    pub(crate) async fn exit(&self) {
        log::info!("[lasper] daemon::exit() sending RPC...");
        // Send exit command and ignore response since daemon will exit immediately
        let _ = self.rpc_call("exit", serde_json::json!({})).await;
        let _ = tokio::fs::remove_file(&self.rpc_sock_path).await;
        let _ = tokio::fs::remove_file(&self.fd_sock_path).await;
        let _ = tokio::fs::remove_dir(self._fd_sock_dir.path()).await;
        log::info!("[lasper] daemon::exit() RPC returned");
    }

    pub(super) async fn nspawn_config(
        &self,
        operation: NspawnConfigOperation,
    ) -> std::io::Result<NspawnConfigResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("nspawn_config", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn system_operation(&self, operation: SystemOperation) -> std::io::Result<()> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc_call("system_operation", params).await?;
        Ok(())
    }

    pub(super) async fn image_remove(
        &self,
        request: ImageRemoveRequest,
    ) -> std::io::Result<ImageControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("image_remove", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn machine_control(
        &self,
        request: MachineControlRequest,
    ) -> std::io::Result<MachineControlOutcome> {
        let params = serde_json::to_value(request)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("machine_control", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn cli_inspect_machine(
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

    pub(super) async fn systemd_unit(
        &self,
        operation: SystemdUnitOperation,
    ) -> std::io::Result<SystemdUnitResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("systemd_unit", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn nvidia_state(
        &self,
        operation: NvidiaStateOperation,
    ) -> std::io::Result<NvidiaStateResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("nvidia_state", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn deployment_state(
        &self,
        operation: DeploymentStateOperation,
    ) -> std::io::Result<DeploymentStateResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("deployment_state", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn submit_deployment(
        &self,
        params: SubmitDeploymentParams,
        artifact_source: Option<std::fs::File>,
    ) -> std::io::Result<RawFd> {
        let socket = self
            .open_fd_channel(FdOperation::SubmitDeployment(Box::new(params)))
            .await?;
        tokio::task::spawn_blocking(move || {
            receive_deployment_stream(&socket, artifact_source.as_ref())
        })
        .await?
    }

    pub(super) async fn deployment_status(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<Option<DeploymentJobSnapshot>> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("deployment_status", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn resolve_deployment_submission(
        &self,
        request_id: crate::application::provisioning::DeploymentRequestId,
    ) -> std::io::Result<Option<DeploymentSubmissionSnapshot>> {
        let params = serde_json::to_value(DeploymentSubmissionRequest { request_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self
            .rpc_call("resolve_deployment_submission", params)
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn acknowledge_deployment_submission(
        &self,
        request_id: crate::application::provisioning::DeploymentRequestId,
    ) -> std::io::Result<()> {
        let params = serde_json::to_value(DeploymentSubmissionRequest { request_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc_call("acknowledge_deployment_submission", params)
            .await?;
        Ok(())
    }

    pub(super) async fn cancel_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("cancel_deployment", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn acknowledge_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<()> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc_call("acknowledge_deployment", params).await?;
        Ok(())
    }

    pub(super) async fn probe_deployment_recovery(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
        expected_revision: u64,
    ) -> std::io::Result<Vec<crate::application::provisioning::DeploymentRecoveryObservation>> {
        let params = serde_json::to_value(ProbeDeploymentRecoveryRequest {
            deployment_id,
            expected_revision,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result: ProbeDeploymentRecoveryResult =
            serde_json::from_value(self.rpc_call("probe_deployment_recovery", params).await?)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if result.deployment_id != deployment_id || result.manifest_revision != expected_revision {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon recovery probe returned a mismatched deployment revision",
            ));
        }
        Ok(result.observations)
    }

    pub(super) async fn reconcile_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc_call("reconcile_deployment", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn release_unresolved_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
        confirmed: bool,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(ReleaseUnresolvedDeploymentRequest {
            deployment_id,
            confirmed,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self
            .rpc_call("release_unresolved_deployment", params)
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn rootfs(&self, operation: RootfsOperation) -> std::io::Result<RootfsResult> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = self
            .send_rpc_request(
                OutboundRpcRequest::Rootfs { id, operation },
                RpcMethod::Rootfs,
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(super) async fn assess_tar_runtime(&self) -> std::io::Result<TarRuntimeAssessment> {
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
        let request_line = SecretBytes::new(
            serde_json::to_vec(&request)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        );
        sock.write_all(request_line.as_slice()).await?;
        sock.write_all(b"\n").await?;

        let std_sock = sock.into_std()?;
        std_sock.set_nonblocking(false)?;
        Ok(std_sock)
    }
}

fn receive_deployment_stream(
    socket: &std::os::unix::net::UnixStream,
    artifact_source: Option<&std::fs::File>,
) -> std::io::Result<RawFd> {
    socket.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    if let Some(source) = artifact_source {
        use std::io::BufRead;

        let mut reader = std::io::BufReader::new(socket.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim() != "artifact-ready" {
            return Err(std::io::Error::other(format!(
                "daemon refused deployment artifact fd: {}",
                line.trim()
            )));
        }
        socket.send_with_fd(b"artifact", &[source.as_raw_fd()])?;
    }

    let mut buffer = [0u8; 512];
    let mut fds = [-1 as RawFd; 1];
    let (read, count) = socket.recv_with_fd(&mut buffer, &mut fds)?;
    if count != 1 {
        for fd in fds.into_iter().take(count).filter(|fd| *fd >= 0) {
            unsafe { libc::close(fd) };
        }
        return Err(std::io::Error::other(format!(
            "daemon deployment submission failed: {}",
            String::from_utf8_lossy(&buffer[..read]).trim()
        )));
    }
    if &buffer[..read] != b"ok" {
        unsafe { libc::close(fds[0]) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon returned an invalid deployment stream response",
        ));
    }
    Ok(fds[0])
}

#[cfg(test)]
mod tests {
    use super::{
        configure_sudo_daemon_stdio, discard_cancelled_responses, rpc_response_timeout,
        wait_for_rpc_response, PendingRpcResponses,
    };
    use crate::daemon::protocol::{RpcFamily, RpcMethod};

    #[tokio::test]
    async fn sudo_daemon_stdio_detaches_input_and_pipes_stdout() {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("test ! -t 0 && test -p /proc/self/fd/1");
        configure_sudo_daemon_stdio(&mut command);

        let mut child = command.spawn().unwrap();
        let mut stdout = child.stdout.take().expect("stdout must be piped");
        tokio::io::copy(&mut stdout, &mut tokio::io::sink())
            .await
            .unwrap();

        assert!(child.wait().await.unwrap().success());
    }

    #[test]
    fn rpc_timeout_policy_keeps_commands_longer_than_coordination_calls() {
        assert_eq!(
            rpc_response_timeout(RpcFamily::Query),
            rpc_response_timeout(RpcFamily::Session)
        );
        assert!(rpc_response_timeout(RpcFamily::Query) < rpc_response_timeout(RpcFamily::Job));
        assert!(rpc_response_timeout(RpcFamily::Job) < rpc_response_timeout(RpcFamily::Command));
    }

    #[tokio::test]
    async fn mutating_rpc_timeout_reports_an_unknown_outcome() {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let error = wait_for_rpc_response(
            response_rx,
            RpcMethod::MachineControl,
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("outcome is unknown"));
        assert!(response_tx.is_closed());
    }

    #[tokio::test]
    async fn job_rpc_timeout_requires_reconciliation() {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let error = wait_for_rpc_response(
            response_rx,
            RpcMethod::CancelDeployment,
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("must be reconciled"));
        assert!(response_tx.is_closed());
    }

    #[test]
    fn pending_rpc_cleanup_removes_only_cancelled_receivers() {
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        let (active_tx, _active_rx) = tokio::sync::oneshot::channel();
        drop(cancelled_rx);
        let mut pending = PendingRpcResponses::from([(1, cancelled_tx), (2, active_tx)]);

        assert_eq!(discard_cancelled_responses(&mut pending), 1);
        assert!(!pending.contains_key(&1));
        assert!(pending.contains_key(&2));
    }
}
