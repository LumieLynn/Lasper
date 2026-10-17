//! Control-channel admission and request/response lifetime.

use crate::application::operations::ResourceReservation;
use crate::application::OperationRegistry;
use crate::daemon::dispatch::backend::{DaemonRuntimeQueries, DaemonSystemExecutor};
use crate::daemon::dispatch::{claims::resource_claims, Dispatcher};
use crate::daemon::server::DaemonShutdown;
use crate::domain::secret::zeroize_string;
use crate::ipc::protocol::{error_code, RpcError, RpcMethod, RpcRequest};
use crate::ipc::transport::{read_bounded_line, MAX_RPC_FRAME_BYTES};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const MAX_RPC_IN_FLIGHT: usize = 64;

pub(in crate::daemon) async fn run_rpc_request_pump<R, B>(
    reader: &mut R,
    out_tx: &mpsc::Sender<String>,
    dispatcher: Dispatcher<B>,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    B: DaemonRuntimeQueries + DaemonSystemExecutor + Clone + 'static,
{
    let request_slots = Arc::new(Semaphore::new(MAX_RPC_IN_FLIGHT));
    let shutdown = Arc::clone(&dispatcher.server_state.shutdown);
    loop {
        let line = tokio::select! {
            biased;
            () = shutdown.requested() => break,
            line = read_bounded_line(reader, MAX_RPC_FRAME_BYTES) => line?,
        };
        let Some(mut line) = line else { break };
        if line.trim().is_empty() {
            zeroize_string(&mut line);
            continue;
        }

        let parsed = serde_json::from_str(&line);
        zeroize_string(&mut line);
        let request: RpcRequest = match parsed {
            Ok(request) => request,
            Err(error) => {
                let error = RpcError::new(error_code::PARSE_ERROR, format!("Parse error: {error}"));
                if !respond(out_tx, &shutdown, None, Err(error)).await {
                    break;
                }
                continue;
            }
        };
        let method = match request.method_kind() {
            Ok(method) => method,
            Err(error) => {
                if !respond(out_tx, &shutdown, Some(request.id), Err(error)).await {
                    break;
                }
                continue;
            }
        };
        let Some(active_request) = shutdown.admit() else {
            break;
        };
        let admission = match admit(
            &request_slots,
            &dispatcher.server_state.operations,
            method,
            &request.params,
        ) {
            Ok(admission) => admission,
            Err(error) => {
                if !respond(out_tx, &shutdown, Some(request.id), Err(error)).await {
                    break;
                }
                continue;
            }
        };
        log::trace!(
            "Daemon RPC: accepted {} method in {} family",
            method.wire_name(),
            method.family().as_str()
        );

        let dispatcher = dispatcher.clone();
        let out_tx = out_tx.clone();
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            // Bound the operation and its response with one request slot.
            // Resource ownership ends when the operation finishes, before its
            // reply can let the client submit a dependent request.
            let (_request_slot, reservation) = admission;
            let result = dispatcher
                .handle(method, request.params)
                .await
                .map_err(|error| RpcError::new(error_code::INTERNAL_ERROR, error));
            drop(reservation);
            drop(active_request);
            respond(&out_tx, &shutdown, Some(request.id), result).await;
        });
    }
    Ok(())
}

fn admit(
    request_slots: &Arc<Semaphore>,
    operations: &Arc<OperationRegistry>,
    method: RpcMethod,
    params: &Value,
) -> Result<(OwnedSemaphorePermit, Option<ResourceReservation>), RpcError> {
    let claims = resource_claims(method, params)
        .map_err(|error| RpcError::new(error_code::INVALID_PARAMS, error))?;
    let reservation = if claims.is_empty() {
        None
    } else {
        Some(operations.reserve(claims).map_err(|conflict| {
            RpcError::new(
                error_code::RESOURCE_BUSY,
                format!("resource is busy: {:?}", conflict.key),
            )
        })?)
    };
    let permit = request_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| RpcError::new(error_code::REQUEST_LIMIT, "daemon request limit reached"))?;
    Ok((permit, reservation))
}

async fn respond(
    out_tx: &mpsc::Sender<String>,
    shutdown: &DaemonShutdown,
    id: Option<u64>,
    result: Result<Value, RpcError>,
) -> bool {
    let response = match result {
        Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => serde_json::json!({"jsonrpc": "2.0", "id": id, "error": error}),
    };
    let line = serde_json::to_string(&response).expect("JSON-RPC values are serializable");
    // Do not wait on a dead or stalled client's response queue during shutdown.
    // Operation and handoff futures themselves are drained, never aborted.
    tokio::select! {
        biased;
        () = shutdown.requested() => false,
        sent = out_tx.send(line) => sent.is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::server::DaemonServerState;
    use crate::ipc::protocol::RpcResponse;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn shutdown_does_not_wait_for_a_full_response_queue() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(DaemonServerState::default());
        let dispatcher = Dispatcher {
            dbus: None::<crate::adapters::runtime::dbus::DbusBackend>,
            invoking_uid: uzers::get_current_uid(),
            server_state: Arc::clone(&state),
            trusted_state_root: crate::adapters::trusted_state::TrustedStateRoot::for_test(
                directory.path().to_path_buf(),
            ),
            machine_sessions: crate::adapters::session::MachineSessionTransport::SystemdTools,
        };
        let (out_tx, _out_rx) = mpsc::channel(1);
        out_tx.send("occupied".into()).await.unwrap();
        let (mut client, server) = tokio::io::duplex(1024);
        let pump = tokio::spawn(async move {
            run_rpc_request_pump(&mut tokio::io::BufReader::new(server), &out_tx, dispatcher).await
        });
        // Block a normal response and the pump's own parse-error response.
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\ninvalid\n")
            .await
            .unwrap();
        tokio::task::yield_now().await;

        state.shutdown.begin();
        tokio::time::timeout(Duration::from_secs(1), async {
            crate::daemon::server::shutdown_daemon_resources(&state).await;
            pump.await.unwrap().unwrap();
        })
        .await
        .expect("shutdown waited on a response queue with no reader");
    }

    #[test]
    fn pending_file_operations_keep_their_request_slots_until_completion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        runtime.block_on(async {
            // Hold file I/O so all accepted requests remain pending. This must
            // also bound handlers that used to spawn a second, detached task.
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            });
            started_rx.await.unwrap();

            let (out_tx, mut out_rx) = mpsc::channel(MAX_RPC_IN_FLIGHT + 1);
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let dispatcher = Dispatcher {
                dbus: None::<crate::adapters::runtime::dbus::DbusBackend>,
                invoking_uid: uzers::get_current_uid(),
                server_state: Arc::new(DaemonServerState::default()),
                trusted_state_root: crate::adapters::trusted_state::TrustedStateRoot::for_test(
                    directory.path().to_path_buf(),
                ),
                machine_sessions: crate::adapters::session::MachineSessionTransport::SystemdTools,
            };
            let pump = tokio::spawn(async move {
                run_rpc_request_pump(&mut tokio::io::BufReader::new(server), &out_tx, dispatcher)
                    .await
            });
            for id in 1..=MAX_RPC_IN_FLIGHT + 1 {
                let request = serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": "nvidia_state",
                    "params": {"operation": "read", "params": {"machine": "test"}}
                });
                client
                    .write_all(format!("{request}\n").as_bytes())
                    .await
                    .unwrap();
                if id < MAX_RPC_IN_FLIGHT {
                    // Process an independent request between file operations.
                    // A burst alone could fill slots before detached handlers
                    // had a chance to return and incorrectly release them.
                    let ping = serde_json::json!({
                        "jsonrpc": "2.0", "id": 1000 + id, "method": "ping"
                    });
                    client
                        .write_all(format!("{ping}\n").as_bytes())
                        .await
                        .unwrap();
                    let response = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    let response: RpcResponse = serde_json::from_str(&response).unwrap();
                    assert_eq!(response.id, (1000 + id) as u64);
                    assert!(response.error.is_none());
                }
            }

            let rejected = tokio::time::timeout(Duration::from_secs(1), out_rx.recv()).await;
            // Release before asserting, so a regression cannot strand a
            // blocking worker during runtime teardown.
            release_tx.send(()).unwrap();
            worker.await.unwrap();
            let rejected: RpcResponse = serde_json::from_str(
                &rejected
                    .expect("pending operations escaped the request limit")
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(rejected.id, (MAX_RPC_IN_FLIGHT + 1) as u64);
            assert_eq!(rejected.error.unwrap().code, error_code::REQUEST_LIMIT);

            let mut completed = std::collections::HashSet::new();
            for _ in 0..MAX_RPC_IN_FLIGHT {
                let response = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let response: RpcResponse = serde_json::from_str(&response).unwrap();
                assert!(
                    completed.insert(response.id),
                    "request replied more than once"
                );
            }
            assert_eq!(completed, (1..=MAX_RPC_IN_FLIGHT as u64).collect());

            client
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"ping\"}\n")
                .await
                .unwrap();
            let response = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let response: RpcResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(response.id, 100);
            assert!(
                response.error.is_none(),
                "finished operations did not release their slots"
            );
            drop(client);
            pump.await.unwrap().unwrap();
        });
    }
}
