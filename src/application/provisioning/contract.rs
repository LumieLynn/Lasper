use crate::domain::nvidia::NvidiaPassthroughProfile;
use crate::domain::secret::SecretString;
use crate::domain::wayland::WaylandGrantIntent;
use crate::nspawn::models::{
    ArtifactSpec, BootstrapSpec, ContainerConfig, DiskImageConfig, OciNetworkMode,
};
use async_trait::async_trait;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

pub(crate) const DEPLOYMENT_EVENT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeploymentId(NonZeroU64);

impl DeploymentId {
    pub(crate) fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentSource {
    Copy {
        source_name: String,
    },
    Oci {
        reference: String,
        read_only: bool,
        network: OciNetworkMode,
    },
    Bootstrap(BootstrapSpec),
    Pull {
        url: String,
        is_raw: bool,
    },
    Artifact(ArtifactSpec),
}

impl DeploymentSource {
    pub fn is_unacknowledged_remote_tar(&self, acknowledged: bool) -> bool {
        matches!(self, Self::Pull { is_raw: false, .. }) && !acknowledged
    }

    pub fn supports_rootfs_configuration(&self) -> bool {
        !matches!(self, Self::Copy { .. } | Self::Oci { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeploymentStorage {
    Directory,
    Subvolume,
    DiskImage(DiskImageConfig),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeploymentRequest {
    pub config: ContainerConfig,
    pub source: DeploymentSource,
    pub storage: DeploymentStorage,
    pub nvidia_profile: Option<NvidiaPassthroughProfile>,
    pub wayland: Vec<WaylandGrantIntent>,
    pub allow_unsafe_remote_tar: bool,
}

impl DeploymentRequest {
    pub(crate) fn validate(&self) -> Result<(), DeploymentError> {
        crate::nspawn::models::NspawnConfigSpec::try_from(&self.config)
            .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        for user in &self.config.users {
            user.validate()
                .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        }
        let mut requested_uids = std::collections::HashSet::new();
        for uid in self.config.users.iter().filter_map(|user| user.uid) {
            if !requested_uids.insert(uid) {
                return Err(DeploymentError::rejected(format!(
                    "multiple users request uid {uid}"
                )));
            }
        }
        let mut wayland_targets = std::collections::HashSet::new();
        let mut wayland_sources = std::collections::HashSet::new();
        for intent in &self.wayland {
            if !self.source.supports_rootfs_configuration() {
                return Err(DeploymentError::rejected(
                    "Wayland grants require a deployment source that supports rootfs user configuration",
                ));
            }
            let Some(user) = self
                .config
                .users
                .iter()
                .find(|user| user.username == intent.target_username())
            else {
                return Err(DeploymentError::rejected(
                    "Wayland target must be one of the users created by this deployment",
                ));
            };
            if !wayland_targets.insert(intent.target_username()) {
                return Err(DeploymentError::rejected(
                    "a container user may have only one Wayland access intent",
                ));
            }
            for source in intent.sources() {
                if !wayland_sources.insert(source.canonical_path()) {
                    return Err(DeploymentError::rejected(
                        "a host Wayland socket may be granted only once",
                    ));
                }
            }
            user.validate()
                .map_err(|error| DeploymentError::rejected(error.to_string()))?;
            if user.uid != Some(intent.required_uid()) {
                return Err(DeploymentError::rejected(format!(
                    "Wayland target {} must request host session uid {}",
                    user.username,
                    intent.required_uid(),
                )));
            }
            super::wayland::validate_wayland_intent(intent, self.config.private_users)?;
        }
        Ok(())
    }
}

pub struct UserSecret {
    username: String,
    password: Option<SecretString>,
}

impl UserSecret {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username,
            password: (!password.is_empty()).then(|| SecretString::new(password)),
        }
    }

    fn validate(&self) -> Result<(), DeploymentError> {
        if let Some(password) = &self.password {
            crate::nspawn::models::validate_chpasswd_secret(
                "user password",
                password.expose_secret(),
            )
            .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        }
        Ok(())
    }

    fn into_password(self) -> Option<SecretString> {
        self.password
    }
}

impl fmt::Debug for UserSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserSecret")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

pub struct DeploymentSecrets {
    root_password: Option<SecretString>,
    users: Vec<UserSecret>,
}

impl DeploymentSecrets {
    pub fn new(root_password: String, users: Vec<UserSecret>) -> Self {
        Self {
            root_password: (!root_password.is_empty()).then(|| SecretString::new(root_password)),
            users,
        }
    }

    pub fn validate_for(&self, config: &ContainerConfig) -> Result<(), DeploymentError> {
        if let Some(password) = &self.root_password {
            crate::nspawn::models::validate_chpasswd_secret(
                "root password",
                password.expose_secret(),
            )
            .map_err(|error| DeploymentError::rejected(error.to_string()))?;
        }

        if self.users.len() != config.users.len()
            || self
                .users
                .iter()
                .zip(&config.users)
                .any(|(secret, user)| secret.username != user.username)
        {
            return Err(DeploymentError::rejected(
                "deployment secrets do not match the requested user accounts",
            ));
        }
        for secret in &self.users {
            secret.validate()?;
        }
        Ok(())
    }

    pub(crate) fn has_account_changes(&self) -> bool {
        self.root_password.is_some() || !self.users.is_empty()
    }

    pub(crate) fn take_root_password(&mut self) -> Option<SecretString> {
        self.root_password.take()
    }

    pub(crate) fn take_user_password(
        &mut self,
        username: &str,
    ) -> Result<Option<SecretString>, DeploymentError> {
        let Some(index) = self
            .users
            .iter()
            .position(|secret| secret.username == username)
        else {
            return Err(DeploymentError::rejected(format!(
                "missing secret capsule entry for user {username:?}"
            )));
        };
        Ok(self.users.remove(index).into_password())
    }
}

impl fmt::Debug for DeploymentSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeploymentSecrets")
            .field(
                "root_password",
                &self.root_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("user_count", &self.users.len())
            .finish()
    }
}

pub struct DeploymentSubmission {
    request: DeploymentRequest,
    secrets: DeploymentSecrets,
}

impl DeploymentSubmission {
    pub fn new(request: DeploymentRequest, secrets: DeploymentSecrets) -> Self {
        Self { request, secrets }
    }

    pub fn request(&self) -> &DeploymentRequest {
        &self.request
    }

    pub(crate) fn into_parts(self) -> (DeploymentRequest, DeploymentSecrets) {
        (self.request, self.secrets)
    }

    pub(crate) fn validate_secrets(&self) -> Result<(), DeploymentError> {
        self.secrets.validate_for(&self.request.config)?;
        if !self.request.source.supports_rootfs_configuration()
            && self.secrets.has_account_changes()
        {
            return Err(DeploymentError::rejected(
                "This deployment source does not support account configuration",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for DeploymentSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeploymentSubmission")
            .field("request", &self.request)
            .field("secrets", &self.secrets)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentPreflight {
    Ready,
    ConfirmationRequired(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTarSafety {
    Compatible,
    Risk(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentProgress {
    pub label: String,
    pub permille: u16,
}

impl DeploymentProgress {
    pub fn new(label: impl Into<String>, permille: u16) -> Self {
        Self {
            label: label.into(),
            permille: permille.min(1000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentEvent {
    Line(String),
    Progress(DeploymentProgress),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentStatus {
    Running,
    RollingBack,
    Succeeded,
    Failed(String),
    Cancelled(String),
}

impl DeploymentStatus {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed(_) | Self::Cancelled(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeploymentErrorKind {
    Rejected,
    Failed,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DeploymentError {
    kind: DeploymentErrorKind,
    message: String,
}

impl DeploymentError {
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            kind: DeploymentErrorKind::Rejected,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: DeploymentErrorKind::Failed,
            message: message.into(),
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            kind: DeploymentErrorKind::Cancelled,
            message: message.into(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == DeploymentErrorKind::Cancelled
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DeploymentCancellation {
    requested: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl DeploymentCancellation {
    pub(crate) fn request(&self) {
        if !self.requested.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub(crate) fn checkpoint(&self) -> Result<(), DeploymentCancellationRequested> {
        if self.is_requested() {
            Err(DeploymentCancellationRequested)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("deployment cancellation requested")]
pub(crate) struct DeploymentCancellationRequested;

#[derive(Clone)]
pub(crate) struct DeploymentJobContext {
    events: mpsc::Sender<DeploymentEvent>,
    status: watch::Sender<DeploymentStatus>,
    cancellation: DeploymentCancellation,
}

impl DeploymentJobContext {
    pub(crate) fn event_sender(&self) -> mpsc::Sender<DeploymentEvent> {
        self.events.clone()
    }

    pub(crate) fn cancellation(&self) -> DeploymentCancellation {
        self.cancellation.clone()
    }

    pub(crate) fn set_rolling_back(&self, rolling_back: bool) {
        self.status.send_replace(if rolling_back {
            DeploymentStatus::RollingBack
        } else {
            DeploymentStatus::Running
        });
    }

    pub(crate) fn status_sender(&self) -> watch::Sender<DeploymentStatus> {
        self.status.clone()
    }
}

pub struct DeploymentJobHandle {
    id: DeploymentId,
    events: mpsc::Receiver<DeploymentEvent>,
    status: watch::Receiver<DeploymentStatus>,
    cancellation: DeploymentCancellation,
}

impl DeploymentJobHandle {
    pub fn id(&self) -> DeploymentId {
        self.id
    }

    pub fn try_recv(&mut self) -> Result<DeploymentEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    pub fn status(&self) -> DeploymentStatus {
        self.status.borrow().clone()
    }

    pub fn request_cancel(&self) {
        self.cancellation.request();
    }

    pub fn cancellation_requested(&self) -> bool {
        self.cancellation.is_requested()
    }
}

pub(crate) fn deployment_job_channel(
    id: DeploymentId,
) -> (DeploymentJobHandle, DeploymentJobContext) {
    let (event_tx, event_rx) = mpsc::channel(DEPLOYMENT_EVENT_CAPACITY);
    let (status_tx, status_rx) = watch::channel(DeploymentStatus::Running);
    let cancellation = DeploymentCancellation::default();
    (
        DeploymentJobHandle {
            id,
            events: event_rx,
            status: status_rx,
            cancellation: cancellation.clone(),
        },
        DeploymentJobContext {
            events: event_tx,
            status: status_tx,
            cancellation,
        },
    )
}

#[async_trait]
pub trait SourcePreflight: Send + Sync + 'static {
    async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError>;
}

#[async_trait]
pub trait DeploymentExecutor: Send + Sync + 'static {
    async fn run(
        &self,
        submission: DeploymentSubmission,
        context: DeploymentJobContext,
    ) -> Result<(), DeploymentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{
        HostWaylandSocket, SocketRevision, WaylandDisplay, WaylandGrantIntent,
    };
    use crate::nspawn::models::CreateUser;

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
            config: ContainerConfig {
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
            config: ContainerConfig {
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
                config: ContainerConfig {
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
            config: ContainerConfig {
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
            config: ContainerConfig {
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
                config: ContainerConfig {
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
        let id = DeploymentId::new(1).unwrap();
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
        let id = DeploymentId::new(1).unwrap();
        let (handle, context) = deployment_job_channel(id);

        assert!(!context.cancellation().is_requested());
        handle.request_cancel();
        assert!(context.cancellation().is_requested());
    }
}
