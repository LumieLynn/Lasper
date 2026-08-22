use crate::domain::nvidia::NvidiaPassthroughProfile;
use crate::domain::secret::SecretString;
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
        self.secrets.validate_for(&self.request.config)
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
    use crate::nspawn::models::CreateUser;

    #[test]
    fn request_debug_and_serializable_config_contain_no_passwords() {
        let request = DeploymentRequest {
            config: ContainerConfig {
                name: "test".into(),
                users: vec![CreateUser {
                    username: "alice".into(),
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
