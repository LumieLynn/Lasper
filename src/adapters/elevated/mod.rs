//! Client for the isolated root daemon.
//!
//! This facade starts the launcher, authenticates IPC, and coordinates shutdown.
//! RPC and FD channels own their transports; the sibling proxy modules translate
//! configuration, runtime, provisioning, and session operations.

mod configuration;
mod fd_channel;
mod launcher;
mod provisioning;
mod rpc;
mod runtime;
mod session;

pub(crate) use fd_channel::pipe_reader_owned;
pub(crate) use launcher::ElevationMethod;

use fd_channel::FdChannel;
use launcher::{DaemonInvocation, DaemonLauncher};
use rpc::RpcClient;

use crate::ipc::transport::create_fd_socket_dir;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) struct ElevatedDaemon {
    rpc: RpcClient,
    fd_channel: FdChannel,
    launcher_exit: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
    pid: u32,
    rpc_sock_path: PathBuf,
    socket_directory: Arc<tempfile::TempDir>,
}

impl std::fmt::Debug for ElevatedDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElevatedDaemon")
            .field("pid", &self.pid)
            .field("fd_sock_path", &self.fd_channel.path())
            .finish_non_exhaustive()
    }
}

impl ElevatedDaemon {
    pub(crate) async fn spawn(
        method: ElevationMethod,
        dbus_enabled: bool,
    ) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let user_uid = uzers::get_current_uid();
        let parent_pid = std::process::id();
        let auth_token: Arc<str> = Arc::from(uuid::Uuid::new_v4().to_string());
        let socket_directory = Arc::new(create_fd_socket_dir(user_uid)?);
        let fd_sock_path = socket_directory.path().join("fd.sock");
        let rpc_sock_path = socket_directory.path().join("rpc.sock");

        let mut launcher = DaemonLauncher::spawn(
            method,
            DaemonInvocation {
                executable: &exe,
                fd_socket: &fd_sock_path,
                rpc_socket: &rpc_sock_path,
                invoking_uid: user_uid,
                frontend_pid: parent_pid,
            },
            Arc::clone(&socket_directory),
        )?;
        let connection: std::io::Result<_> = async {
            let mut stream = launcher.connect_rpc_socket(&rpc_sock_path).await?;
            let pid = rpc::authenticate(&mut stream, &auth_token, dbus_enabled).await?;
            Ok((stream, pid))
        }
        .await;
        let (stream, pid) = match connection {
            Ok(connection) => connection,
            Err(error) => return Err(with_cleanup_error(error, launcher.abort().await)),
        };

        let (rpc, connection) = RpcClient::new(stream);
        let (launcher_exit_tx, launcher_exit) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            // run() releases both socket halves and every pending response.
            // Only then may we wait for the daemon to observe EOF and exit.
            connection.run().await;
            let result = launcher.finish().await.map_err(|error| error.to_string());
            if let Err(error) = &result {
                log::error!("Elevated daemon cleanup failed: {error}");
            }
            let _ = launcher_exit_tx.send(Some(result));
        });

        let daemon = Self {
            rpc,
            fd_channel: FdChannel::new(fd_sock_path, auth_token),
            launcher_exit,
            pid,
            rpc_sock_path,
            socket_directory,
        };
        if let Err(error) = daemon.rpc.call("ping", serde_json::json!({})).await {
            let error = std::io::Error::new(
                error.kind(),
                format!("daemon health check failed after {method} launch: {error}"),
            );
            return Err(with_cleanup_error(error, daemon.finish_shutdown().await));
        }

        log::info!("Elevated daemon ready (pid={pid})");
        Ok(daemon)
    }

    pub(super) fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.rpc.subscribe_events()
    }

    pub(super) async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<serde_json::Value> {
        self.rpc.call(method, params).await
    }

    pub(crate) async fn exit(&self) -> std::io::Result<()> {
        // The daemon may close RPC before replying. Launcher completion, rather
        // than this response, confirms whether the process has been reaped.
        let _ = self.rpc.call("exit", serde_json::json!({})).await;
        self.finish_shutdown().await
    }

    async fn finish_shutdown(&self) -> std::io::Result<()> {
        self.rpc.shutdown();
        let mut completion = self.launcher_exit.clone();
        let result = match completion.wait_for(Option::is_some).await {
            Ok(result) => result
                .as_ref()
                .expect("launcher finished")
                .clone()
                .map_err(std::io::Error::other),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "daemon launcher completion channel closed; process exit is unconfirmed",
            )),
        };
        let _ = tokio::fs::remove_file(&self.rpc_sock_path).await;
        let _ = tokio::fs::remove_file(self.fd_channel.path()).await;
        let _ = tokio::fs::remove_dir(self.socket_directory.path()).await;
        result
    }
}

fn with_cleanup_error(error: std::io::Error, cleanup: std::io::Result<()>) -> std::io::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => std::io::Error::new(
            error.kind(),
            format!("{error}; launcher cleanup failed: {cleanup}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{RpcRequest, RpcResponse};
    use crate::ipc::transport::{read_bounded_line, MAX_RPC_FRAME_BYTES};
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn exit_closes_client_io_without_waiting_for_server_eof() {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (rpc, connection) = RpcClient::new(client);
        let (completed_tx, completed_rx) = tokio::sync::watch::channel(None);
        let io = tokio::spawn(async move {
            connection.run().await;
            completed_tx.send(Some(Ok(()))).unwrap();
        });
        let server = tokio::spawn(async move {
            let (reader, mut writer) = server.into_split();
            let mut reader = BufReader::new(reader);
            let line = read_bounded_line(&mut reader, MAX_RPC_FRAME_BYTES)
                .await
                .unwrap()
                .unwrap();
            let request: RpcRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "exit");
            let response = RpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id,
                result: Some(serde_json::Value::Null),
                error: None,
            };
            writer
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
            // Keep the server open until the client closes its own socket.
            assert!(read_bounded_line(&mut reader, MAX_RPC_FRAME_BYTES)
                .await
                .unwrap()
                .is_none());
        });
        let daemon = ElevatedDaemon {
            rpc,
            fd_channel: FdChannel::new(directory.path().join("fd.sock"), Arc::from("test")),
            launcher_exit: completed_rx,
            pid: 0,
            rpc_sock_path: directory.path().join("rpc.sock"),
            socket_directory: directory,
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), daemon.exit())
            .await
            .unwrap()
            .unwrap();
        io.await.unwrap();
        server.await.unwrap();
    }
}
