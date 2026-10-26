use super::job::DEPLOYMENT_EVENT_CAPACITY;
use super::*;
use crate::domain::machine::{CreateUser, MachineConfig};
use crate::domain::provisioning::bootstrap::BootstrapSpec;
use crate::domain::provisioning::oci::OciNetworkMode;
use crate::domain::provisioning::{
    BootstrapDeployment, CloneDeployment, DeploymentSpec, DeploymentStorage, ImageImport,
    ImportDeployment, ImportSource, OciDeployment,
};
use tokio::sync::mpsc;

fn bootstrap() -> DeploymentSpec {
    DeploymentSpec::Bootstrap(Box::new(BootstrapDeployment {
        machine: MachineConfig {
            name: "test".into(),
            ..Default::default()
        },
        provider: BootstrapSpec::Pacstrap(Default::default()),
        storage: DeploymentStorage::Directory,
        users: vec![CreateUser {
            username: "alice".into(),
            ..Default::default()
        }],
        wayland: Vec::new(),
        integration: Default::default(),
    }))
}

#[test]
fn submission_debug_redacts_all_secrets() {
    let submission = DeploymentSubmission::new(
        bootstrap(),
        DeploymentSecrets::new(
            "root-secret".into(),
            vec![UserSecret::new("alice".into(), "user-secret".into())],
        ),
    );
    submission.validate_secrets().unwrap();
    let debug = format!("{submission:?}");
    assert!(!debug.contains("root-secret"));
    assert!(!debug.contains("user-secret"));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn bootstrap_requires_secrets_to_match_the_requested_accounts() {
    let submission = DeploymentSubmission::new(
        bootstrap(),
        DeploymentSecrets::new(
            String::new(),
            vec![UserSecret::new("bob".into(), String::new())],
        ),
    );
    assert!(submission.validate_secrets().is_err());
}

#[test]
fn preserved_systems_reject_account_initialization_secrets() {
    let specs = [
        DeploymentSpec::Import(Box::new(ImportDeployment {
            machine: MachineConfig {
                name: "test".into(),
                ..Default::default()
            },
            image: ImageImport::DiskImage {
                source: ImportSource::Local {
                    path: "/tmp/rootfs.raw".into(),
                },
            },
            integration: Default::default(),
        })),
        DeploymentSpec::Clone(CloneDeployment {
            name: "test".into(),
            source_name: "base".into(),
        }),
        DeploymentSpec::Oci(OciDeployment {
            name: "test".into(),
            reference: "docker.io/library/alpine:latest".into(),
            read_only: false,
            network: OciNetworkMode::Host,
        }),
    ];
    for spec in specs {
        for secrets in [
            DeploymentSecrets::new("root-secret".into(), Vec::new()),
            DeploymentSecrets::new(
                String::new(),
                vec![UserSecret::new("alice".into(), "user-secret".into())],
            ),
        ] {
            let error = DeploymentSubmission::new(spec.clone(), secrets)
                .validate_secrets()
                .unwrap_err();
            assert!(error.to_string().contains("Only Bootstrap"));
        }
    }
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
