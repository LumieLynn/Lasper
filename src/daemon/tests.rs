use super::dispatch::handler::*;
use super::dispatch::*;
use super::server::logging::*;
use super::server::*;
use crate::adapters::process::open_pidfd;
use crate::adapters::provisioning::engine::image_operation::TarRuntimeAssessment;
use crate::adapters::rootfs::store::RootfsOperation;
use crate::adapters::system_operation::{SystemOperation, SystemOperationError};
use crate::application::image_lifecycle::{ImageRemoveRequest, ImageRemoveTransport};
use crate::application::machine_lifecycle::{
    MachineControlTransport, MachineRuntimeAction, MachineRuntimeControlRequest,
    NspawnLaunchRequest, NspawnUnitAction, NspawnUnitControlRequest,
};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::{AllowedSignal, MachineName};
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry};
use crate::domain::session::SessionSize;
use crate::ipc::protocol::rootfs as rootfs_wire;
use crate::ipc::protocol::session::{self as session, SpawnTerminalParams};
use crate::ipc::protocol::*;
use crate::ipc::transport::*;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

const TEST_TOKEN: &str = "f865fd7e-a9f5-4ef1-b5b5-f3f257a75ce0";

#[derive(Clone)]
struct SlowRemoveDbus {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl DaemonRuntimeQueries for SlowRemoveDbus {
    async fn list_machines(&self) -> crate::application::runtime::RuntimeResult<Vec<MachineEntry>> {
        Err(crate::application::runtime::RuntimeError::failed(
            "slow test backend does not list machines",
        ))
    }

    async fn list_images(&self) -> crate::application::runtime::RuntimeResult<Vec<ImageEntry>> {
        Err(crate::application::runtime::RuntimeError::failed(
            "slow test backend does not list images",
        ))
    }

    async fn get_properties(
        &self,
        _name: &str,
        _include_nspawn_unit: bool,
    ) -> crate::application::runtime::RuntimeResult<MachineProperties> {
        Err(crate::application::runtime::RuntimeError::failed(
            "slow test backend does not inspect machines",
        ))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl DaemonSystemExecutor for SlowRemoveDbus {
    async fn system_operation(
        &self,
        operation: SystemOperation,
    ) -> crate::adapters::system_operation::SystemOperationResult<()> {
        match operation {
            SystemOperation::RemoveImage { .. } => {
                self.started.notify_one();
                self.release.notified().await;
                Ok(())
            }
            _ => Err(SystemOperationError::Backend(
                "slow test backend only handles image removal".into(),
            )),
        }
    }
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
fn rootfs_rpc_uses_the_typed_outbound_envelope() {
    let wire_operation: rootfs_wire::RootfsOperation = serde_json::from_value(serde_json::json!({
        "operation": "set_root_password",
        "params": {
            "target": {"kind": "machine", "machine": "test"},
            "password": "wire-sentinel"
        }
    }))
    .unwrap();
    let operation = RootfsOperation::try_from(wire_operation).unwrap();
    let bytes = OutboundRpcRequest::General(RpcRequest {
        jsonrpc: "2.0".into(),
        id: 41,
        method: "rootfs".into(),
        params: serde_json::to_value(rootfs_wire::RootfsOperation::from(operation)).unwrap(),
    })
    .into_wire_bytes()
    .unwrap();
    let request: RpcRequest = serde_json::from_slice(bytes.as_slice()).unwrap();

    assert_eq!(request.id, 41);
    assert_eq!(request.method, "rootfs");
    assert_eq!(
        request.params["params"]["password"].as_str(),
        Some("wire-sentinel")
    );
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

#[test]
fn machine_runtime_rpc_is_typed_and_claims_the_machine_resource() {
    let request = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "machine_runtime_control".into(),
        params: serde_json::to_value(MachineRuntimeControlRequest {
            machine: MachineName::new("test-machine").unwrap(),
            action: MachineRuntimeAction::Kill {
                signal: AllowedSignal::Kill,
            },
            transport: MachineControlTransport::Dbus,
        })
        .unwrap(),
    };

    assert_eq!(
        daemon_resource_claim(&request).unwrap(),
        Some(crate::application::ResourceClaim::exclusive(
            crate::application::ResourceKey::Nspawn("test-machine".into())
        ))
    );
    let mut invalid = request.params;
    invalid["program"] = serde_json::json!("sh");
    assert!(serde_json::from_value::<MachineRuntimeControlRequest>(invalid).is_err());
}

#[test]
fn nspawn_launch_and_unit_requests_share_the_image_resource_identity() {
    let image = ImageName::new("test-image").unwrap();
    let machine = MachineName::new("test-image").unwrap();
    let expected = Some(crate::application::ResourceClaim::exclusive(
        crate::application::ResourceKey::Nspawn("test-image".into()),
    ));
    let launch = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "nspawn_launch".into(),
        params: serde_json::to_value(NspawnLaunchRequest {
            image: image.clone(),
            machine: machine.clone(),
            transport: MachineControlTransport::Dbus,
        })
        .unwrap(),
    };
    let unit = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 2,
        method: "nspawn_unit_control".into(),
        params: serde_json::to_value(NspawnUnitControlRequest {
            machine,
            action: NspawnUnitAction::Enable,
            transport: MachineControlTransport::Dbus,
        })
        .unwrap(),
    };

    assert_eq!(daemon_resource_claim(&launch).unwrap(), expected);
    assert_eq!(daemon_resource_claim(&unit).unwrap(), expected);

    let mismatched = RpcRequest {
        params: serde_json::to_value(NspawnLaunchRequest {
            image,
            machine: MachineName::new("another-machine").unwrap(),
            transport: MachineControlTransport::Dbus,
        })
        .unwrap(),
        ..launch
    };
    assert!(daemon_resource_claim(&mismatched).is_err());
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
async fn daemon_keeps_dbus_capability_when_the_bus_is_late() {
    assert!(initialize_dbus_backend(true).await.is_some());
}

#[tokio::test]
async fn slow_remove_image_does_not_block_independent_requests() {
    let slow = SlowRemoveDbus {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let started = slow.started.clone();
    let release = slow.release.clone();
    let dbus = Some(slow);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(4);
    let remove = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "image_remove".into(),
        params: serde_json::to_value(ImageRemoveRequest {
            image: crate::domain::runtime::ImageName::new("slow-image").unwrap(),
            transport: ImageRemoveTransport::Dbus,
        })
        .unwrap(),
    };
    let ping = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 2,
        method: "ping".into(),
        params: serde_json::json!({}),
    };
    let input = format!(
        "{}\n{}\n",
        serde_json::to_string(&remove).unwrap(),
        serde_json::to_string(&ping).unwrap()
    );
    let (mut client, server) = tokio::io::duplex(4096);
    let mut reader = tokio::io::BufReader::new(server);
    let server_state = Arc::new(DaemonServerState::default());

    // This is the production request pump. A slow non-cancelable image
    // removal must not prevent an independent in-memory query from being
    // read and completed.
    let mut pump = tokio::spawn(async move {
        run_rpc_request_pump(
            &mut reader,
            &dbus,
            &out_tx,
            uzers::get_current_uid(),
            server_state,
            crate::adapters::trusted_state::TrustedStateRoot::production(),
        )
        .await
    });
    use tokio::io::AsyncWriteExt;
    client.write_all(input.as_bytes()).await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("RemoveImage did not reach the slow executor");
    let ping_response = tokio::time::timeout(std::time::Duration::from_millis(250), out_rx.recv())
        .await
        .expect("ping response remained blocked behind RemoveImage")
        .expect("request pump stopped before ping responded");
    let ping_response: RpcResponse = serde_json::from_str(&ping_response).unwrap();
    assert_eq!(ping_response.id, 2);
    assert!(ping_response.error.is_none());

    release.notify_one();
    let remove_response = tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv())
        .await
        .expect("RemoveImage response was not emitted")
        .expect("request pump stopped before RemoveImage responded");
    let remove_response: RpcResponse = serde_json::from_str(&remove_response).unwrap();
    assert_eq!(remove_response.id, 1);
    assert!(remove_response.error.is_none());

    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut pump)
        .await
        .expect("request pump did not stop after input EOF")
        .expect("request pump task panicked")
        .expect("request pump returned an error");
}

#[tokio::test]
async fn slow_remove_image_rejects_same_resource_start_promptly() {
    let slow = SlowRemoveDbus {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let started = slow.started.clone();
    let release = slow.release.clone();
    let dbus = Some(slow);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(4);
    let image = crate::domain::runtime::ImageName::new("slow-image").unwrap();
    let remove = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "image_remove".into(),
        params: serde_json::to_value(ImageRemoveRequest {
            image: image.clone(),
            transport: ImageRemoveTransport::Dbus,
        })
        .unwrap(),
    };
    let start = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 2,
        method: "nspawn_launch".into(),
        params: serde_json::to_value(NspawnLaunchRequest {
            image: ImageName::new(image.as_str()).unwrap(),
            machine: crate::domain::machine::MachineName::new(image.as_str()).unwrap(),
            transport: MachineControlTransport::Dbus,
        })
        .unwrap(),
    };
    let (mut client, server) = tokio::io::duplex(4096);
    let mut reader = tokio::io::BufReader::new(server);
    let server_state = Arc::new(DaemonServerState::default());
    let mut pump = tokio::spawn(async move {
        run_rpc_request_pump(
            &mut reader,
            &dbus,
            &out_tx,
            uzers::get_current_uid(),
            server_state,
            crate::adapters::trusted_state::TrustedStateRoot::production(),
        )
        .await
    });
    use tokio::io::AsyncWriteExt;
    client
        .write_all(format!("{}\n", serde_json::to_string(&remove).unwrap()).as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("image removal did not acquire its daemon claim");
    client
        .write_all(format!("{}\n", serde_json::to_string(&start).unwrap()).as_bytes())
        .await
        .unwrap();

    let response = tokio::time::timeout(std::time::Duration::from_millis(250), out_rx.recv())
        .await
        .expect("same-resource start waited behind image removal")
        .expect("request pump stopped before rejecting start");
    let response: RpcResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(response.id, 2);
    assert_eq!(response.error.unwrap().code, -32002);

    release.notify_one();
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv())
        .await
        .expect("image removal did not finish")
        .expect("request pump stopped before removal response");
    let response: RpcResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(response.id, 1);
    assert!(response.error.is_none());

    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut pump)
        .await
        .expect("request pump did not stop after input EOF")
        .expect("request pump task panicked")
        .expect("request pump returned an error");
}

#[test]
fn machine_inspection_rpc_accepts_only_a_typed_target() {
    let valid: InspectMachineRequest = serde_json::from_value(serde_json::json!({
        "machine": "test-machine",
        "include_nspawn_unit": true
    }))
    .unwrap();
    assert_eq!(valid.machine.as_str(), "test-machine");
    assert!(valid.include_nspawn_unit);

    assert!(serde_json::from_value::<InspectMachineRequest>(
        serde_json::json!({"machine": "../escape", "include_nspawn_unit": true})
    )
    .is_err());
    assert!(
        serde_json::from_value::<InspectMachineRequest>(serde_json::json!({
            "machine": "test-machine",
            "include_nspawn_unit": false,
            "unexpected": true
        }))
        .is_err()
    );
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
    let request = r#"{"method":"spawn_journalctl","params":{"session_id":1,"name":"test"}}"#;
    assert!(serde_json::from_str::<FdRequest>(request).is_err());
}

#[test]
fn fd_request_round_trip_uses_typed_terminal_parameters() {
    let request = FdRequest {
        auth_token: TEST_TOKEN.to_string(),
        operation: FdOperation::Terminal(SpawnTerminalParams {
            session_id: session::WireSessionId::new(7).unwrap(),
            name: MachineName::new("test-machine").unwrap(),
            size: SessionSize::new(120, 40).unwrap().into(),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "spawn_terminal");
    assert_eq!(json["params"]["session_id"], 7);
    assert_eq!(json["params"]["name"], "test-machine");
    assert_eq!(json["params"]["size"]["cols"], 120);
    assert_eq!(json["params"]["size"]["rows"], 40);

    let parsed: FdRequest = serde_json::from_value(json).unwrap();
    match parsed.operation {
        FdOperation::Terminal(params) => {
            assert_eq!(params.session_id.get(), 7);
            assert_eq!(params.name.as_str(), "test-machine");
            assert_eq!(params.size, SessionSize::new(120, 40).unwrap().into());
        }
        _ => panic!("expected spawn_terminal"),
    }
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
fn dbus_inspection_request_revalidates_machine_name() {
    let valid: InspectMachineRequest = serde_json::from_value(serde_json::json!({
        "machine": "valid-machine",
        "include_nspawn_unit": false
    }))
    .unwrap();
    assert_eq!(valid.machine.as_str(), "valid-machine");

    assert!(
        serde_json::from_value::<InspectMachineRequest>(serde_json::json!({
            "machine": "../escape",
            "include_nspawn_unit": false
        }))
        .is_err()
    );
}

#[tokio::test]
async fn tar_runtime_assessment_rpc_returns_typed_result_and_rejects_parameters() {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(1);
    let server_state = Arc::new(DaemonServerState::default());
    let request = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 7,
        method: "assess_tar_runtime".into(),
        params: serde_json::json!({}),
    };

    assert!(matches!(
        handle_request(
            request,
            &None::<crate::adapters::runtime::dbus::DbusBackend>,
            &out_tx,
            uzers::get_current_uid(),
            Arc::clone(&server_state),
            crate::adapters::trusted_state::TrustedStateRoot::production(),
        )
        .await,
        HandleOutcome::Spawned
    ));
    let response: RpcResponse = serde_json::from_str(&out_rx.recv().await.unwrap()).unwrap();
    assert!(response.error.is_none());
    let _: TarRuntimeAssessment = serde_json::from_value(response.result.unwrap()).unwrap();

    let invalid = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 8,
        method: "assess_tar_runtime".into(),
        params: serde_json::json!({"program": "tar"}),
    };
    assert!(matches!(
        handle_request(
            invalid,
            &None::<crate::adapters::runtime::dbus::DbusBackend>,
            &out_tx,
            uzers::get_current_uid(),
            server_state,
            crate::adapters::trusted_state::TrustedStateRoot::production(),
        )
        .await,
        HandleOutcome::Sync(Err(error)) if error.contains("does not accept parameters")
    ));
}

#[tokio::test]
async fn deployment_recovery_probe_reloads_the_trusted_manifest_revision() {
    use crate::adapters::provisioning::state::FilesystemDeploymentState;
    use crate::application::provisioning::MachineProvisioningConfig;
    use crate::application::provisioning::{
        DeploymentCrashManifest, DeploymentId, DeploymentPlan, DeploymentRequest, DeploymentSource,
        DeploymentStatePort, DeploymentStorage,
    };
    use crate::ipc::protocol::deployment::{
        ProbeDeploymentRecoveryRequest, ProbeDeploymentRecoveryResult,
    };

    let directory = tempfile::tempdir().unwrap();
    let root =
        crate::adapters::trusted_state::TrustedStateRoot::for_test(directory.path().join("lasper"));
    let state = FilesystemDeploymentState::new(root.clone());
    let plan = DeploymentPlan::build(DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "recovery-target".into(),
            ..Default::default()
        },
        source: DeploymentSource::Copy {
            source_name: "base".into(),
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: Vec::new(),
        allow_unsafe_remote_tar: false,
    })
    .unwrap();
    let deployment_id = DeploymentId::from_u128(88);
    let manifest = DeploymentCrashManifest::prepared(deployment_id, &plan);
    state.create(manifest.clone()).await.unwrap();
    let (out_tx, _out_rx) = tokio::sync::mpsc::channel(1);
    let server_state = Arc::new(DaemonServerState::default());

    let request = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 9,
        method: "probe_deployment_recovery".into(),
        params: serde_json::to_value(ProbeDeploymentRecoveryRequest {
            deployment_id,
            expected_revision: manifest.revision,
        })
        .unwrap(),
    };
    let result = match handle_request(
        request,
        &None::<crate::adapters::runtime::dbus::DbusBackend>,
        &out_tx,
        uzers::get_current_uid(),
        Arc::clone(&server_state),
        root.clone(),
    )
    .await
    {
        HandleOutcome::Sync(Ok(result)) => result,
        _ => panic!("recovery probe should return synchronously"),
    };
    let result: ProbeDeploymentRecoveryResult = serde_json::from_value(result).unwrap();
    assert_eq!(result.deployment_id, deployment_id);
    assert_eq!(result.manifest_revision, manifest.revision);
    assert!(result.observations.is_empty());

    let stale = RpcRequest {
        jsonrpc: "2.0".into(),
        id: 10,
        method: "probe_deployment_recovery".into(),
        params: serde_json::to_value(ProbeDeploymentRecoveryRequest {
            deployment_id,
            expected_revision: manifest.revision + 1,
        })
        .unwrap(),
    };
    match handle_request(
        stale,
        &None::<crate::adapters::runtime::dbus::DbusBackend>,
        &out_tx,
        uzers::get_current_uid(),
        server_state,
        root,
    )
    .await
    {
        HandleOutcome::Sync(Err(error)) => {
            assert!(error.contains("revision changed"), "{error}")
        }
        HandleOutcome::Sync(Ok(result)) => panic!("stale recovery probe succeeded: {result}"),
        HandleOutcome::Spawned => panic!("stale recovery probe unexpectedly spawned a task"),
    }
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

    let _ready = tokio::time::timeout(std::time::Duration::from_secs(1), async_pidfd.readable())
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
