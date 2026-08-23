use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

use super::super::state::DeploymentStateSession;
use super::identity::DeploymentId;
use super::secrets::DeploymentSecrets;

pub(crate) const DEPLOYMENT_EVENT_CAPACITY: usize = 256;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum DeploymentEvent {
    Line(String),
    Progress(DeploymentProgress),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum DeploymentStatus {
    Running,
    RollingBack,
    Succeeded,
    Failed(String),
    Cancelled(String),
    ReconciliationRequired(String),
}

impl DeploymentStatus {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed(_)
                | Self::Cancelled(_)
                | Self::ReconciliationRequired(_)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentClaimStatus {
    Held,
    Released,
    ReconciliationRequired,
    Reconciled,
    ReleasedUnresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeploymentErrorKind {
    Rejected,
    Failed,
    Cancelled,
    ReconciliationRequired,
    ReconciledUnknown,
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

    pub(crate) fn reconciliation_required(message: impl Into<String>) -> Self {
        Self {
            kind: DeploymentErrorKind::ReconciliationRequired,
            message: message.into(),
        }
    }

    pub(crate) fn reconciled_unknown(message: impl Into<String>) -> Self {
        Self {
            kind: DeploymentErrorKind::ReconciledUnknown,
            message: message.into(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == DeploymentErrorKind::Cancelled
    }

    pub(crate) fn requires_reconciliation(&self) -> bool {
        matches!(
            self.kind,
            DeploymentErrorKind::ReconciliationRequired | DeploymentErrorKind::ReconciledUnknown
        )
    }

    pub(crate) fn retains_resource_claim(&self) -> bool {
        self.kind == DeploymentErrorKind::ReconciliationRequired
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
    id: DeploymentId,
    events: mpsc::Sender<DeploymentEvent>,
    status: watch::Sender<DeploymentStatus>,
    claim_status: watch::Sender<DeploymentClaimStatus>,
    cancellation: DeploymentCancellation,
    state: Option<DeploymentStateSession>,
}

impl DeploymentJobContext {
    pub(crate) fn id(&self) -> DeploymentId {
        self.id
    }

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

    pub(crate) fn claim_status_sender(&self) -> watch::Sender<DeploymentClaimStatus> {
        self.claim_status.clone()
    }

    pub(crate) fn state_session(&self) -> Option<DeploymentStateSession> {
        self.state.clone()
    }

    pub(crate) fn with_state_session(mut self, state: DeploymentStateSession) -> Self {
        self.state = Some(state);
        self
    }
}

pub struct DeploymentJobHandle {
    id: DeploymentId,
    events: mpsc::Receiver<DeploymentEvent>,
    status: watch::Receiver<DeploymentStatus>,
    claim_status: watch::Receiver<DeploymentClaimStatus>,
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

    pub fn claim_status(&self) -> DeploymentClaimStatus {
        *self.claim_status.borrow()
    }

    pub fn request_cancel(&self) {
        self.cancellation.request();
    }

    pub fn cancellation_requested(&self) -> bool {
        self.cancellation.is_requested()
    }

    pub(crate) fn cancellation(&self) -> DeploymentCancellation {
        self.cancellation.clone()
    }

    pub(crate) fn into_streams(
        self,
    ) -> (
        mpsc::Receiver<DeploymentEvent>,
        watch::Receiver<DeploymentStatus>,
    ) {
        (self.events, self.status)
    }
}

pub(crate) fn deployment_job_channel(
    id: DeploymentId,
) -> (DeploymentJobHandle, DeploymentJobContext) {
    deployment_job_channel_inner(id, None)
}

fn deployment_job_channel_inner(
    id: DeploymentId,
    state: Option<DeploymentStateSession>,
) -> (DeploymentJobHandle, DeploymentJobContext) {
    let (event_tx, event_rx) = mpsc::channel(DEPLOYMENT_EVENT_CAPACITY);
    let (status_tx, status_rx) = watch::channel(DeploymentStatus::Running);
    let (claim_status_tx, claim_status_rx) = watch::channel(DeploymentClaimStatus::Held);
    let cancellation = DeploymentCancellation::default();
    (
        DeploymentJobHandle {
            id,
            events: event_rx,
            status: status_rx,
            claim_status: claim_status_rx,
            cancellation: cancellation.clone(),
        },
        DeploymentJobContext {
            id,
            events: event_tx,
            status: status_tx,
            claim_status: claim_status_tx,
            cancellation,
            state,
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
        plan: super::super::state::DeploymentPlan,
        secrets: DeploymentSecrets,
        context: DeploymentJobContext,
    ) -> Result<(), DeploymentError>;
}

#[async_trait]
pub(crate) trait DeploymentClaimControl: Send + Sync + 'static {
    async fn release_unresolved(
        &self,
        deployment_id: DeploymentId,
        confirmed: bool,
    ) -> Result<(), DeploymentError>;
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryDeploymentClaimControl;

#[cfg(test)]
#[async_trait]
impl DeploymentClaimControl for MemoryDeploymentClaimControl {
    async fn release_unresolved(
        &self,
        _deployment_id: DeploymentId,
        confirmed: bool,
    ) -> Result<(), DeploymentError> {
        if confirmed {
            Ok(())
        } else {
            Err(DeploymentError::rejected(
                "releasing an unresolved deployment requires explicit confirmation",
            ))
        }
    }
}
