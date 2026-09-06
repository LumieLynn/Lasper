//! Root daemon bootstrap, authenticated listeners, and FD-passing handlers.

mod fd;
pub(crate) mod logging;
mod process_state;
mod state;

use self::logging::{initialize_daemon_logging, AuthLogLimiter};
use super::dispatch::run_rpc_request_pump;
use crate::adapters::runtime::source::RuntimeSource;
use crate::domain::secret::zeroize_string;
use crate::ipc::protocol::*;
use crate::ipc::transport::{
    authorize_fd_peer, authorize_fd_token, configure_user_socket, get_peer_credentials,
    read_bounded_line, FdAuthorizationError, PeerCredentials, MAX_RPC_FRAME_BYTES,
};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

const MAX_FD_CONNECTIONS: usize = 32;

#[derive(Clone)]
struct FdHostContext {
    trusted_state_root: crate::adapters::trusted_state::TrustedStateRoot,
    machine_sessions: crate::adapters::session::MachineSessionTransport,
    invoking_uid: u32,
}

pub(super) async fn initialize_dbus_backend(
    enabled: bool,
) -> Option<crate::adapters::runtime::dbus::DbusBackend> {
    enabled.then(crate::adapters::runtime::dbus::DbusBackend::new)
}

pub(super) fn require_daemon_root(effective_uid: u32) -> std::io::Result<()> {
    if effective_uid == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "lasper daemon must run as root",
        ))
    }
}

fn monitor_parent(pidfd: OwnedFd, state: Arc<DaemonServerState>) -> std::io::Result<()> {
    let pidfd = tokio::io::unix::AsyncFd::new(pidfd)?;
    tokio::spawn(async move {
        match pidfd.readable().await {
            Ok(_) => log::info!("Daemon: launching TUI exited; stopping elevated daemon"),
            Err(error) => log::error!("Daemon: parent pidfd monitor failed: {error}"),
        }
        shutdown_daemon_resources(&state).await;
        std::process::exit(0);
    });
    Ok(())
}

pub(super) struct AuthenticatedRpcConnection {
    pub(super) reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    pub(super) writer: tokio::net::unix::OwnedWriteHalf,
    pub(super) auth_token: Arc<str>,
    pub(super) dbus_enabled: bool,
}

pub(super) async fn accept_rpc_connection(
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
        let mut line = match tokio::time::timeout(
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
        let parsed = serde_json::from_str(&line);
        zeroize_string(&mut line);
        let bootstrap: RpcBootstrap = match parsed {
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
    crate::adapters::process::enable_daemon_child_lifecycle();
    if let Err(error) = initialize_daemon_logging() {
        eprintln!("lasper daemon: failed to initialize logging: {error}");
        std::process::exit(1);
    }
    if expected_parent_pid == 0 {
        log::error!("Daemon: missing launching TUI PID");
        std::process::exit(1);
    }
    let server_state = Arc::new(DaemonServerState::default());
    let parent_pidfd = match crate::adapters::process::open_pidfd(expected_parent_pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            log::error!("Daemon: failed to pin launching TUI process: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = monitor_parent(parent_pidfd, Arc::clone(&server_state)) {
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
    let writer_state = Arc::clone(&server_state);
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
        shutdown_daemon_resources(&writer_state).await;
        std::process::exit(0);
    });

    let dbus = initialize_dbus_backend(dbus_enabled).await;
    let machine_sessions = match dbus.as_ref() {
        Some(dbus) => crate::adapters::session::MachineSessionTransport::Dbus(dbus.clone()),
        None => crate::adapters::session::MachineSessionTransport::Cli,
    };
    let trusted_state_root = crate::adapters::trusted_state::TrustedStateRoot::production();

    if let Some(ref dbus) = dbus {
        let out_tx_bg = out_tx.clone();
        let dbus_bg = dbus.clone();
        tokio::spawn(async move {
            let (ev_tx, mut ev_rx) =
                tokio::sync::mpsc::channel::<crate::domain::runtime::StatusUpdate>(16);
            let watcher = tokio::spawn(async move {
                let mut retry_delay = std::time::Duration::from_secs(1);
                loop {
                    match dbus_bg.watch_events(ev_tx.clone()).await {
                        Ok(()) if ev_tx.is_closed() => break,
                        Ok(()) => {
                            log::warn!("Daemon D-Bus watcher stopped; reconnecting");
                        }
                        Err(error) => {
                            log::warn!("Daemon D-Bus watcher unavailable: {error}; reconnecting");
                        }
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
                }
            });
            while ev_rx.recv().await.is_some() {
                let notif = serde_json::json!({"jsonrpc":"2.0","method":"dbus_event","params":{}});
                let line = serde_json::to_string(&notif).unwrap();
                if out_tx_bg.send(line).await.is_err() {
                    break;
                }
            }
            watcher.abort();
        });
    }

    let fd_slots = Arc::new(tokio::sync::Semaphore::new(MAX_FD_CONNECTIONS));
    let fd_server_state = Arc::clone(&server_state);
    let fd_host = FdHostContext {
        trusted_state_root: trusted_state_root.clone(),
        machine_sessions: machine_sessions.clone(),
        invoking_uid: user_uid,
    };
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
                        Arc::clone(&fd_server_state),
                        fd_host.clone(),
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

    let exit_code = match run_rpc_request_pump(
        &mut rpc_reader,
        &dbus,
        &out_tx,
        user_uid,
        Arc::clone(&server_state),
        trusted_state_root,
        machine_sessions,
    )
    .await
    {
        Ok(()) => 0,
        Err(error) => {
            log::error!("Daemon RPC socket error: {error}");
            1
        }
    };
    shutdown_daemon_resources(&server_state).await;
    std::process::exit(exit_code);
}

// ── Daemon side fd-passing handler ──

async fn handle_fd_connection(
    stream: UnixStream,
    expected_peer: PeerCredentials,
    expected_auth_token: Arc<str>,
    auth_log: AuthLogLimiter,
    server_state: Arc<DaemonServerState>,
    host: FdHostContext,
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
    let mut line = match tokio::time::timeout(
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

    let parsed = serde_json::from_str(line.trim());
    zeroize_string(&mut line);
    let request: FdRequest = match parsed {
        Ok(v) => v,
        Err(e) => {
            if let Some(message) = auth_log.record(format!("Daemon fd-handler: parse error: {e}")) {
                log::warn!("{message}");
            }
            return;
        }
    };
    let FdRequest {
        mut auth_token,
        operation,
    } = request;
    let authenticated = authorize_fd_token(&auth_token, &expected_auth_token).is_ok();
    zeroize_string(&mut auth_token);
    if !authenticated {
        if let Some(message) = auth_log.record(format!(
            "Daemon fd-handler: rejected unauthenticated request from pid {}",
            actual_peer.pid
        )) {
            log::warn!("{message}");
        }
        return;
    }

    let stream = buf_reader.into_inner();

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

    self::fd::handle(
        std_stream,
        operation,
        server_state,
        host.trusted_state_root,
        host.machine_sessions,
        host.invoking_uid,
    )
    .await;
}

pub(crate) use process_state::shutdown_daemon_resources;
pub(crate) use state::DaemonServerState;
