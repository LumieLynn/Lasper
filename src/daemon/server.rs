//! Root daemon bootstrap, authenticated listeners, and FD-passing handlers.

use super::dispatch::{initialize_dbus_backend, run_rpc_request_pump};
use super::logging::{initialize_daemon_logging, AuthLogLimiter};
use super::process_state::{
    shutdown_daemon_resources, stop_unhanded_command, OCI_TRANSFER_CANCELLATIONS, SPAWN_EXIT_CODES,
    SPAWN_PIDS, SPAWN_WAIT_ERRORS,
};
use super::protocol::*;
use super::session_server::{self, DaemonServerState};
use super::transport::{
    authorize_fd_peer, authorize_fd_token, configure_user_socket, get_peer_credentials,
    read_bounded_line, FdAuthorizationError, PeerCredentials, MAX_RPC_FRAME_BYTES,
};
use crate::adapters::provisioning::engine::bootstrap_operation::{
    build_command as build_bootstrap_command, probe_debootstrap_signature_style_sync,
    validate_target as validate_bootstrap_target,
};
use crate::adapters::provisioning::engine::image_operation::ImageImportReport;
use crate::adapters::provisioning::engine::oci_operation::{
    run_oci_transfer, OciTransferCancellation, OciTransferOutcome,
};
use crate::adapters::runtime::source::RuntimeSource;
use crate::domain::secret::zeroize_string;
use sendfd::{RecvWithFd, SendWithFd};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

const MAX_FD_CONNECTIONS: usize = 32;

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

pub(super) fn open_pidfd(pid: u32) -> std::io::Result<OwnedFd> {
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
    let parent_pidfd = match open_pidfd(expected_parent_pid) {
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
    let fd_server_state = Arc::clone(&server_state);
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
        FdOperation::Journalctl(params) => {
            session_server::spawn_journal(&mut std_stream, params, server_state);
        }

        FdOperation::Terminal(params) => {
            session_server::spawn_terminal(&mut std_stream, params, server_state);
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
            let mut command = crate::adapters::process::new_sync_command("sh");
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

            let result =
                crate::adapters::provisioning::engine::image_operation::import_raw_system_image(
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
            let result = crate::adapters::provisioning::engine::image_operation::import_tar_image(
                request, source,
            )
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

pub(super) fn image_import_response(
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
