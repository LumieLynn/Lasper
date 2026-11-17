//! Prepared workers keep their lease while callers are idle. Dropping the
//! session requests cleanup; snapshots remain available to observers.
//!
//! BrokerSession sends commands to a background driver that owns the connection,
//! renews the prepared lease and observes cleanup. Dropping the command sender
//! wakes that driver; snapshot receivers and data descriptors do not retain it.

mod driver;
mod error;
mod prepare;
mod start;
mod wire;

pub use error::BrokerSessionError;
pub(crate) use prepare::EncodedPrepare;
pub use start::{StartError, StartOutcome};

use crate::{BrokerConnection, WorkerObservation};
use lasper_guest_protocol::{PreparationId, PrepareRequest, TerminalSize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{timeout_at, Duration, Instant};

/// Host-observed progress. WireSessionState separately validates frame ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Prepared,
    /// Start is queued for sending. Cancelling can no longer guarantee that
    /// the worker will not execute.
    Starting,
    /// The guest accepted Start; only Exit can establish whether exec succeeded.
    Accepted,
    /// Cleanup is being observed; progress reports do not extend its deadline.
    Closing,
    Finished,
}

/// The latest submission facts, validated guest report and host observation error.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub phase: SessionPhase,
    pub submission: StartSubmission,
    pub report: Option<lasper_guest_protocol::ExitReport>,
    pub error: Option<Arc<BrokerSessionError>>,
}

impl SessionSnapshot {
    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    /// Retains the first host-side failure even if cleanup later completes.
    /// A finished session can have both an error and a final Exit.
    pub fn error(&self) -> Option<&BrokerSessionError> {
        self.error.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartSubmission {
    NotSubmitted,
    Unconfirmed,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub submission: StartSubmission,
    pub report: Option<lasper_guest_protocol::ExitReport>,
    pub error: Option<Arc<BrokerSessionError>>,
}

#[derive(Clone, Debug, Default)]
pub struct StartOptions {
    pub terminal_size: Option<TerminalSize>,
    pub execution_timeout: Option<Duration>,
}

/// Dropping the handle requests cleanup but does not wait for it. The host
/// observes cleanup for the configured budget; the guardian remains responsible
/// for reaping after the host stops waiting.
#[must_use]
pub struct BrokerSession {
    preparation: PreparationId,
    observation: WorkerObservation,
    io: Option<crate::PreparedIo>,
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<SessionSnapshot>,
    timeouts: crate::ClientTimeouts,
}

type Reply<T> = oneshot::Sender<Result<T, BrokerSessionError>>;
type StartReply = oneshot::Sender<Result<StartOutcome, StartError>>;

// The task can be aborted before its first poll or while awaiting I/O. Publish
// the end of observation to every subscriber without claiming guest cleanup.
struct DriverObservation(watch::Sender<SessionSnapshot>);

impl Drop for DriverObservation {
    fn drop(&mut self) {
        self.0.send_modify(|snapshot| {
            if snapshot.phase != SessionPhase::Finished {
                snapshot.phase = SessionPhase::Finished;
                snapshot
                    .error
                    .get_or_insert_with(|| Arc::new(BrokerSessionError::DriverStopped));
            }
        });
    }
}

enum Command {
    Recheck {
        deadline: Instant,
        reply: Reply<WorkerObservation>,
    },
    Start {
        deadline: Instant,
        options: StartOptions,
        reply: StartReply,
        delivered: oneshot::Receiver<()>,
    },
    Cancel {
        deadline: Instant,
        reply: Reply<()>,
    },
    Close {
        deadline: Instant,
        reply: Reply<()>,
    },
}

impl Command {
    fn deadline(&self) -> Instant {
        match self {
            Self::Recheck { deadline, .. }
            | Self::Start { deadline, .. }
            | Self::Cancel { deadline, .. }
            | Self::Close { deadline, .. } => *deadline,
        }
    }

    fn is_abandoned(&self) -> bool {
        match self {
            Self::Recheck { reply, .. } => reply.is_closed(),
            Self::Start { reply, .. } => reply.is_closed(),
            Self::Cancel { reply, .. } | Self::Close { reply, .. } => reply.is_closed(),
        }
    }

    async fn closed(&mut self) {
        match self {
            Self::Recheck { reply, .. } => reply.closed().await,
            Self::Start { reply, .. } => reply.closed().await,
            Self::Cancel { reply, .. } | Self::Close { reply, .. } => reply.closed().await,
        }
    }

    fn reject(self, error: BrokerSessionError) {
        match self {
            Self::Recheck { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Start { reply, .. } => {
                let _ = reply.send(Err(StartError::NotSubmitted(error)));
            }
            Self::Cancel { reply, .. } | Self::Close { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

impl BrokerSession {
    /// The driver retains an encoded copy of the request so later identity
    /// checks do not depend on the caller keeping its data alive. Preparation
    /// and delivery to the caller share a five-second budget, shortened by
    /// the supplied deadline. Cancelling does not wait for guest cleanup.
    pub async fn prepare(
        connection: BrokerConnection,
        request: PrepareRequest<'_>,
        deadline: Instant,
    ) -> Result<Self, BrokerSessionError> {
        let timeouts = crate::ClientTimeouts::default();
        let deadline = deadline.min(Instant::now() + timeouts.prepare);
        if Instant::now() >= deadline {
            return Err(BrokerSessionError::Deadline);
        }
        let encoded = EncodedPrepare::encode(&connection, request)?;
        Self::prepare_encoded(connection, encoded, deadline, timeouts)
            .await
            .map_err(|(error, _)| error)
    }

    pub(crate) async fn prepare_encoded(
        connection: BrokerConnection,
        encoded: EncodedPrepare,
        deadline: Instant,
        timeouts: crate::ClientTimeouts,
    ) -> Result<Self, (BrokerSessionError, SessionSnapshot)> {
        let (state, snapshots) = watch::channel(SessionSnapshot {
            phase: SessionPhase::Prepared,
            submission: StartSubmission::NotSubmitted,
            report: None,
            error: None,
        });
        let last_snapshot = snapshots.clone();
        let result = async {
            let deadline = deadline.min(Instant::now() + timeouts.prepare);
            if Instant::now() >= deadline {
                return Err(BrokerSessionError::Deadline);
            }
            let EncodedPrepare { header, payload } = encoded;
            let (commands, receiver) = mpsc::channel(1);
            let (ready, received) = oneshot::channel();
            let observation = DriverObservation(state.clone());
            let driver_timeouts = timeouts.clone();
            tokio::spawn(async move {
                let _observation = observation;
                driver::run(
                    connection,
                    header,
                    payload,
                    receiver,
                    state,
                    ready,
                    deadline,
                    driver_timeouts,
                )
                .await;
            });
            let (preparation, observation, io) = timeout_at(deadline, received)
                .await
                .map_err(|_| BrokerSessionError::Deadline)?
                .map_err(|_| BrokerSessionError::DriverStopped)??;
            if Instant::now() >= deadline {
                return Err(BrokerSessionError::Deadline);
            }
            Ok(Self {
                preparation,
                observation,
                io: Some(io),
                commands,
                state: snapshots,
                timeouts,
            })
        }
        .await;
        result.map_err(|error| (error, last_snapshot.borrow().clone()))
    }

    pub fn take_io(&mut self) -> Option<crate::PreparedIo> {
        self.io.take()
    }

    pub fn preparation_id(&self) -> PreparationId {
        self.preparation
    }

    pub fn observation(&self) -> &WorkerObservation {
        &self.observation
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.state.borrow().clone()
    }

    /// Subscriptions keep the last snapshot readable, but do not keep a worker
    /// prepared after the session handle is dropped.
    pub fn subscribe(&self) -> watch::Receiver<SessionSnapshot> {
        self.state.clone()
    }

    /// Intermediate updates may be skipped. If this returns DriverStopped,
    /// `snapshot()` still contains the driver's last report or failure.
    pub async fn changed(&mut self) -> Result<SessionSnapshot, BrokerSessionError> {
        self.state
            .changed()
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)?;
        Ok(self.snapshot())
    }

    /// A cancelled recheck leaves the caller's previous observation unchanged.
    /// Queueing and identity verification share the operation budget.
    pub async fn recheck(&mut self) -> Result<&WorkerObservation, BrokerSessionError> {
        self.recheck_until(Instant::now() + self.timeouts.operation)
            .await
    }

    pub(crate) async fn recheck_until(
        &mut self,
        deadline: Instant,
    ) -> Result<&WorkerObservation, BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Recheck {
            deadline: deadline.min(Instant::now() + self.timeouts.operation),
            reply,
        })
        .await?;
        self.observation = received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)??;
        Ok(&self.observation)
    }

    /// Queueing and identity checks share the operation budget.
    /// Acceptance does not confirm exec success.
    ///
    /// Before Start is queued for sending, an observed cancellation preserves
    /// preparation. Cancellation racing with that point requests Close, since
    /// the worker may already execute. A lost acknowledgement is reported as
    /// Unknown and is never retried. Err means this attempt was not submitted
    /// or was explicitly rejected. Losing the driver after queueing is Unknown.
    pub async fn start(&mut self, options: StartOptions) -> Result<StartOutcome, StartError> {
        self.start_until(options, Instant::now() + self.timeouts.operation)
            .await
    }

    pub(crate) async fn start_until(
        &mut self,
        options: StartOptions,
        deadline: Instant,
    ) -> Result<StartOutcome, StartError> {
        let (reply, received) = oneshot::channel();
        let (delivered, delivery) = oneshot::channel();
        self.send(Command::Start {
            deadline: deadline.min(Instant::now() + self.timeouts.operation),
            options,
            reply,
            delivered: delivery,
        })
        .await
        .map_err(StartError::NotSubmitted)?;
        start::receive(received, delivered).await
    }

    pub async fn cancel(&self) -> Result<(), BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Cancel {
            deadline: Instant::now() + self.timeouts.cleanup_observation,
            reply,
        })
        .await?;
        received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)?
    }

    /// Ok confirms the cleanup request, not that descendants have been reaped.
    /// Continue observing snapshots for the final Exit. Without it, the
    /// cleanup deadline ends observation with Finished and a Deadline error.
    pub async fn close(&self) -> Result<(), BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Close {
            deadline: Instant::now() + self.timeouts.cleanup_observation,
            reply,
        })
        .await?;
        received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)?
    }

    pub async fn wait(&self) -> Completion {
        let mut state = self.state.clone();
        loop {
            let snapshot = state.borrow().clone();
            if snapshot.phase == SessionPhase::Finished || state.has_changed().is_err() {
                return Completion {
                    submission: snapshot.submission,
                    report: snapshot.report,
                    error: snapshot.error.or_else(|| {
                        snapshot
                            .report
                            .filter(|r| r.is_final())
                            .is_none()
                            .then(|| Arc::new(BrokerSessionError::DriverStopped))
                    }),
                };
            }
            let _ = state.changed().await;
        }
    }

    async fn send(&self, command: Command) -> Result<(), BrokerSessionError> {
        timeout_at(command.deadline(), self.commands.send(command))
            .await
            .map_err(|_| BrokerSessionError::Deadline)?
            .map_err(|_| BrokerSessionError::DriverStopped)
    }
}
