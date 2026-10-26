use super::*;
use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
use crate::domain::x11::{HostX11Socket, X11BindIntent, X11SocketRevision};

fn bootstrap() -> BootstrapDeployment {
    BootstrapDeployment {
        machine: MachineConfig {
            name: "test".into(),
            ..Default::default()
        },
        provider: BootstrapSpec::Pacstrap(Default::default()),
        storage: DeploymentStorage::Directory,
        users: vec![CreateUser {
            username: "alice".into(),
            uid: Some(1001),
            ..Default::default()
        }],
        wayland: Vec::new(),
        integration: Default::default(),
    }
}

fn wayland_intent(username: &str) -> WaylandGrantIntent {
    let socket = HostWaylandSocket::from_verified_parts(
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
    WaylandGrantIntent::new(username, vec![socket]).unwrap()
}

#[test]
fn wayland_requires_a_requested_account_with_the_host_session_uid() {
    let mut deployment = bootstrap();
    deployment.wayland = vec![wayland_intent("bob")];
    let error = DeploymentSpec::Bootstrap(Box::new(deployment.clone()))
        .validate()
        .unwrap_err();
    assert!(error.to_string().contains("one of the users created"));

    deployment.wayland = vec![wayland_intent("alice")];
    deployment.users[0].uid = Some(1000);
    let error = DeploymentSpec::Bootstrap(Box::new(deployment.clone()))
        .validate()
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("must request host session uid 1001"));

    deployment.users[0].uid = Some(1001);
    DeploymentSpec::Bootstrap(Box::new(deployment))
        .validate()
        .unwrap();
}

#[test]
fn bootstrap_rejects_duplicate_account_names_and_requested_uids() {
    let mut deployment = bootstrap();
    deployment.users.push(deployment.users[0].clone());
    let error = DeploymentSpec::Bootstrap(Box::new(deployment.clone()))
        .validate()
        .unwrap_err();
    assert!(error.to_string().contains("multiple users request name"));
    deployment.users[1].username = "bob".into();
    let error = DeploymentSpec::Bootstrap(Box::new(deployment))
        .validate()
        .unwrap_err();
    assert!(error.to_string().contains("multiple users request uid"));
}

#[test]
fn imported_systems_reject_account_fields_on_the_wire() {
    let spec = DeploymentSpec::Import(Box::new(ImportDeployment {
        machine: bootstrap().machine,
        image: ImageImport::DiskImage {
            source: ImportSource::Local {
                path: "/tmp/rootfs.raw".into(),
            },
        },
        integration: Default::default(),
    }));
    let mut wire = serde_json::to_value(&spec).unwrap();
    wire["settings"]["users"] = serde_json::json!([]);
    assert!(serde_json::from_value::<DeploymentSpec>(wire).is_err());
}

#[test]
fn imports_validate_format_and_remote_archive_acknowledgement() {
    let spec = |image| {
        DeploymentSpec::Import(Box::new(ImportDeployment {
            machine: bootstrap().machine,
            image,
            integration: Default::default(),
        }))
    };
    let archive = |source, allow_unsafe_remote_tar| ImageImport::Archive {
        source,
        storage: DeploymentStorage::Directory,
        allow_unsafe_remote_tar,
    };
    let remote = ImportSource::Remote {
        url: "https://example.test/rootfs.tar".into(),
    };
    let unconfirmed = spec(archive(remote.clone(), false));
    unconfirmed.validate().unwrap();
    assert!(unconfirmed.needs_remote_tar_confirmation());
    let confirmed = spec(archive(remote, true));
    confirmed.validate().unwrap();
    assert!(!confirmed.needs_remote_tar_confirmation());
    assert!(spec(archive(
        ImportSource::Local {
            path: "/tmp/rootfs.tar".into()
        },
        true
    ))
    .validate()
    .is_err());
    assert!(spec(archive(
        ImportSource::Local {
            path: "/tmp/rootfs.raw".into()
        },
        false
    ))
    .validate()
    .is_err());
    assert!(spec(ImageImport::DiskImage {
        source: ImportSource::Remote {
            url: "file:///tmp/rootfs.raw".into()
        },
    })
    .validate()
    .is_err());
}

#[test]
fn integration_roundtrips_existing_cdi_and_rejects_relative_paths() {
    let mut deployment = bootstrap();
    deployment.integration.nvidia_cdi_source =
        NvidiaCdiSource::existing(Some("relative/nvidia.yaml".into()));
    let error = DeploymentSpec::Bootstrap(Box::new(deployment.clone()))
        .validate()
        .unwrap_err();
    assert!(error.to_string().contains("must be absolute"));
    deployment.integration.nvidia_cdi_source =
        NvidiaCdiSource::existing(Some("/var/run/cdi/nvidia.yaml".into()));
    let spec = DeploymentSpec::Bootstrap(Box::new(deployment));
    let wire = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        wire["settings"]["integration"]["nvidia_cdi_source"]["mode"],
        "existing"
    );
    assert_eq!(
        wire["settings"]["integration"]["nvidia_cdi_source"]["path"],
        "/var/run/cdi/nvidia.yaml"
    );
    assert_eq!(
        serde_json::from_value::<DeploymentSpec>(wire).unwrap(),
        spec
    );
}

#[test]
fn x11_directory_and_individual_sockets_are_mutually_exclusive() {
    let socket = HostX11Socket::from_verified_parts(
        0,
        false,
        "/tmp/.X11-unix/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        1000,
        1000,
        0o777,
        42,
        1000,
        1000,
        X11SocketRevision {
            device: 1,
            inode: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
    )
    .unwrap();
    let mut deployment = bootstrap();
    deployment.machine.x11_binds = vec![
        X11BindIntent::from_socket(socket, "/tmp/.X11-unix/X0".into()).unwrap(),
        X11BindIntent::directory("/tmp/.X11-unix".into(), "/tmp/.X11-unix".into()).unwrap(),
    ];
    let error = DeploymentSpec::Bootstrap(Box::new(deployment))
        .validate()
        .unwrap_err();
    assert!(error.to_string().contains("cannot be combined"));
}
