//! RPC request queue, response routing, and connection shutdown.
//!
//! The driver owns every RPC I/O handle. Once run() returns, the caller can
//! reap the launcher without keeping the daemon's connection open.

use crate::domain::secret::SecretBytes;
use crate::ipc::protocol::{
    error_code, OutboundRpcRequest, RpcBootstrap, RpcCall, RpcError, RpcFamily, RpcMethod,
    RpcRequest, RpcResponse, RPC_PROTOCOL_VERSION,
};
use crate::ipc::transport::{
    authorize_root_server, get_peer_credentials, read_bounded_line, MAX_RPC_FRAME_BYTES,
};
use std::sync::{atomic::AtomicU64, Arc};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{unix::OwnedWriteHalf, UnixStream};
use tokio::sync::{broadcast, mpsc, Notify};

const RPC_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) async fn authenticate(
    stream: &mut UnixStream,
    auth_token: &str,
    dbus_enabled: bool,
) -> std::io::Result<u32> {
    let peer = get_peer_credentials(stream)?;
    authorize_root_server(peer)?;
    let bootstrap = RpcBootstrap {
        protocol_version: RPC_PROTOCOL_VERSION,
        auth_token: auth_token.to_string(),
        dbus_enabled,
    };
    let line = SecretBytes::new(serde_json::to_vec(&bootstrap).map_err(|error| {
        std::io::Error::other(format!("serialize RPC authentication: {error}"))
    })?);
    stream.write_all(line.as_slice()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(peer.pid)
}

pub(super) struct RpcClient {
    request_tx: mpsc::Sender<RpcCall>,
    next_id: AtomicU64,
    event_tx: broadcast::Sender<()>,
    shutdown: Arc<Notify>,
}

pub(super) struct RpcConnection {
    stream: UnixStream,
    requests: mpsc::Receiver<RpcCall>,
    events: broadcast::Sender<()>,
    shutdown: Arc<Notify>,
}

impl RpcClient {
    pub(super) fn new(stream: UnixStream) -> (Self, RpcConnection) {
        let (request_tx, requests) = mpsc::channel(8);
        let (events, _) = broadcast::channel(16);
        let shutdown = Arc::new(Notify::new());
        let client = Self {
            request_tx,
            next_id: AtomicU64::new(1),
            event_tx: events.clone(),
            shutdown: Arc::clone(&shutdown),
        };
        let connection = RpcConnection {
            stream,
            requests,
            events,
            shutdown,
        };
        (client, connection)
    }

    pub(super) fn subscribe_events(&self) -> broadcast::Receiver<()> {
        self.event_tx.subscribe()
    }

    pub(super) fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    pub(super) async fn call(
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
            return Err(std::io::Error::other(err));
        }
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    }
}

impl RpcConnection {
    pub(super) async fn run(self) {
        // Keep shutdown outside the exchange loop so it also interrupts a
        // socket write blocked on an unresponsive daemon. Dropping exchange
        // closes both socket halves and all pending/queued response senders.
        tokio::select! {
            _ = self.shutdown.notified() => {},
            _ = exchange(self.stream, self.requests, self.events) => {},
        }
    }
}

async fn exchange(
    stream: UnixStream,
    mut requests: mpsc::Receiver<RpcCall>,
    events: broadcast::Sender<()>,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let mut pending = PendingRpcResponses::new();
    let mut sweep = tokio::time::interval(std::time::Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = sweep.tick() => {
                let discarded = discard_cancelled_responses(&mut pending);
                if discarded > 0 {
                    log::debug!("Daemon I/O: discarded {discarded} cancelled response slots");
                }
            }
            request = requests.recv() => {
                let Some(request) = request else { break };
                if let Err(error) = write_request(&mut writer, request, &mut pending).await {
                    log::error!("Daemon I/O: failed to write RPC request: {error}");
                    break;
                }
            }
            line = read_bounded_line(&mut reader, MAX_RPC_FRAME_BYTES) => {
                match line {
                    Ok(Some(line)) => {
                        if let Err(error) = route_response(&line, &mut pending, &events) {
                            log::error!("Daemon I/O: failed to parse response: {error}");
                            break;
                        }
                    }
                    Ok(None) => {
                        log::debug!("Daemon I/O: RPC socket closed (EOF)");
                        break;
                    }
                    Err(error) => {
                        log::error!("Daemon I/O: failed to read RPC response: {error}");
                        break;
                    }
                }
            }
        }
    }
}

async fn write_request(
    writer: &mut BufWriter<OwnedWriteHalf>,
    (request, response_tx): RpcCall,
    pending: &mut PendingRpcResponses,
) -> std::io::Result<()> {
    if response_tx.is_closed() {
        return Ok(());
    }
    let id = request.id();
    let line = match request.into_wire_bytes() {
        Ok(bytes) => bytes,
        Err(error) => {
            log::error!("Daemon I/O: failed to serialize request: {error}");
            return Ok(());
        }
    };
    if line.len().saturating_add(1) > MAX_RPC_FRAME_BYTES {
        let _ = response_tx.send(RpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code: error_code::INTERNAL_ERROR,
                message: format!("daemon request exceeds {MAX_RPC_FRAME_BYTES} bytes"),
            }),
        });
        return Ok(());
    }
    writer.write_all(line.as_slice()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    if !response_tx.is_closed() {
        pending.insert(id, response_tx);
    }
    Ok(())
}

fn route_response(
    line: &str,
    pending: &mut PendingRpcResponses,
    events: &broadcast::Sender<()>,
) -> std::io::Result<()> {
    let raw: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            log::error!("Daemon I/O: failed to parse JSON: {error}");
            return Ok(());
        }
    };
    if raw.get("method").is_some() && raw.get("id").is_none() {
        let _ = events.send(());
        return Ok(());
    }
    let Some(id) = raw.get("id").and_then(serde_json::Value::as_u64) else {
        log::warn!("Daemon I/O: unexpected JSON format");
        return Ok(());
    };
    let Some(response_tx) = pending.remove(&id) else {
        log::warn!("Daemon I/O: received response for unknown id={id}");
        return Ok(());
    };
    let response = serde_json::from_value(raw)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let _ = response_tx.send(response);
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_releases_an_inflight_response_and_closes_the_socket() {
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let (client, mut server) = UnixStream::pair().unwrap();
            let (rpc, connection) = RpcClient::new(client);
            let rpc = Arc::new(rpc);
            let driver = tokio::spawn(connection.run());
            let request = {
                let rpc = Arc::clone(&rpc);
                tokio::spawn(async move {
                    rpc.call(
                        "ping",
                        serde_json::json!({ "payload": "x".repeat(MAX_RPC_FRAME_BYTES / 2) }),
                    )
                    .await
                })
            };
            // Leave the peer undrained once the write has begun. Shutdown must
            // interrupt either a blocked write or a pending reply, then close
            // the transport before the launcher starts waiting for process exit.
            server.read_exact(&mut [0]).await.unwrap();
            rpc.shutdown();
            driver.await.unwrap();
            let error = request.await.unwrap().unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            server.read_to_end(&mut Vec::new()).await.unwrap();
        })
        .await
        .expect("RPC shutdown stalled with an inflight request");
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
            RpcMethod::MachineRuntimeControl,
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
