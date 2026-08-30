use super::job::DEPLOYMENT_EVENT_CAPACITY;
use super::MachineProvisioningConfig;
use super::*;
use crate::domain::provisioning::CreateUser;
use crate::domain::provisioning::OciNetworkMode;
use crate::domain::wayland::{
    HostWaylandSocket, SocketRevision, WaylandDisplay, WaylandGrantIntent,
};
use tokio::sync::mpsc;

fn wayland_intent(target_username: &str) -> WaylandGrantIntent {
    let source = HostWaylandSocket::from_verified_parts(
        WaylandDisplay::new("wayland-0").unwrap(),
        "/run/user/1001".into(),
        "/run/user/1001/wayland-0".into(),
        1001,
        1001,
        1001,
        0o755,
        SocketRevision {
            device: 1,
            inode: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
    )
    .unwrap();
    WaylandGrantIntent::new(
        target_username,
        vec![source.clone()],
        source.display().clone(),
    )
    .unwrap()
}

#[test]
fn wayland_target_must_belong_to_the_deployment_user_set() {
    let request = DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "test".into(),
            users: vec![CreateUser {
                username: "alice".into(),
                uid: Some(1001),
                shell: "/bin/bash".into(),
                sudoer: false,
            }],
            ..Default::default()
        },
        source: DeploymentSource::Pull {
            url: "https://example.test/rootfs.raw".into(),
            is_raw: true,
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: vec![wayland_intent("bob")],
        allow_unsafe_remote_tar: false,
    };

    let error = request.validate().unwrap_err();
    assert!(error.to_string().contains("one of the users created"));
}

#[test]
fn wayland_target_must_request_the_host_session_uid() {
    let request = DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "test".into(),
            users: vec![CreateUser {
                username: "alice".into(),
                uid: Some(1000),
                shell: "/bin/bash".into(),
                sudoer: false,
            }],
            ..Default::default()
        },
        source: DeploymentSource::Pull {
            url: "https://example.test/rootfs.raw".into(),
            is_raw: true,
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: vec![wayland_intent("alice")],
        allow_unsafe_remote_tar: false,
    };

    let error = request.validate().unwrap_err();
    assert!(error
        .to_string()
        .contains("must request host session uid 1001"));
}

#[test]
fn wayland_grant_rejects_sources_that_skip_rootfs_configuration() {
    for source in [
        DeploymentSource::Copy {
            source_name: "base".into(),
        },
        DeploymentSource::Oci {
            reference: "docker.io/library/ubuntu:latest".into(),
            read_only: false,
            network: OciNetworkMode::Host,
        },
    ] {
        let request = DeploymentRequest {
            config: MachineProvisioningConfig {
                name: "test".into(),
                users: vec![CreateUser {
                    username: "alice".into(),
                    uid: Some(1001),
                    shell: "/bin/bash".into(),
                    sudoer: false,
                }],
                ..Default::default()
            },
            source,
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: vec![wayland_intent("alice")],
            allow_unsafe_remote_tar: false,
        };

        let error = request.validate().unwrap_err();
        assert!(error
            .to_string()
            .contains("supports rootfs user configuration"));
    }
}

#[test]
fn request_debug_and_serializable_config_contain_no_passwords() {
    let request = DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "test".into(),
            users: vec![CreateUser {
                username: "alice".into(),
                uid: None,
                shell: "/bin/bash".into(),
                sudoer: false,
            }],
            ..Default::default()
        },
        source: DeploymentSource::Copy {
            source_name: "base".into(),
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: Vec::new(),
        allow_unsafe_remote_tar: false,
    };
    let debug = format!("{request:?}");
    let json = serde_json::to_string(&request.config).unwrap();

    assert!(!debug.contains("root-secret"));
    assert!(!debug.contains("user-secret"));
    assert!(!json.contains("password"));
}

#[test]
fn submission_debug_redacts_all_secrets() {
    let request = DeploymentRequest {
        config: MachineProvisioningConfig {
            name: "test".into(),
            users: vec![CreateUser {
                username: "alice".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        source: DeploymentSource::Copy {
            source_name: "base".into(),
        },
        storage: DeploymentStorage::Directory,
        nvidia_profile: None,
        wayland: Vec::new(),
        allow_unsafe_remote_tar: false,
    };
    let submission = DeploymentSubmission::new(
        request,
        DeploymentSecrets::new(
            "root-secret".into(),
            vec![UserSecret::new("alice".into(), "user-secret".into())],
        ),
    );
    let debug = format!("{submission:?}");

    assert!(!debug.contains("root-secret"));
    assert!(!debug.contains("user-secret"));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn sources_without_rootfs_configuration_reject_account_secrets() {
    let submission = DeploymentSubmission::new(
        DeploymentRequest {
            config: MachineProvisioningConfig {
                name: "test".into(),
                ..Default::default()
            },
            source: DeploymentSource::Copy {
                source_name: "base".into(),
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: Vec::new(),
            allow_unsafe_remote_tar: false,
        },
        DeploymentSecrets::new("root-secret".into(), Vec::new()),
    );

    let error = submission.validate_secrets().unwrap_err();
    assert!(error
        .to_string()
        .contains("does not support account configuration"));
}

#[test]
fn job_event_stream_is_bounded() {
    let id = DeploymentId::from_u128(1);
    let (_handle, context) = deployment_job_channel(id);
    let events = context.event_sender();

    for index in 0..DEPLOYMENT_EVENT_CAPACITY {
        events
            .try_send(DeploymentEvent::Line(index.to_string()))
            .unwrap();
    }
    assert!(matches!(
        events.try_send(DeploymentEvent::Line("overflow".into())),
        Err(mpsc::error::TrySendError::Full(_))
    ));
}

#[test]
fn job_handle_propagates_cancellation_to_the_job_context() {
    let id = DeploymentId::from_u128(1);
    let (handle, context) = deployment_job_channel(id);

    assert!(!context.cancellation().is_requested());
    handle.request_cancel();
    assert!(context.cancellation().is_requested());
}
