//! Prepared workers keep their lease while callers are idle. Dropping the
//! session requests cleanup; snapshots remain available to observers.

mod driver;
mod error;
mod output;
mod wire;

pub use error::BrokerSessionError;
pub use output::{CapturedStream, SessionExit, SessionOutput};

use crate::{BrokerConnection, WorkerObservation};
use lasper_guest_protocol::{
    ControlFrameHeader, PreparationId, PrepareRequest, ProjectionObservations, TerminalSize,
};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{timeout_at, Duration, Instant};

const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const KEEP_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Prepared,
    /// Start is queued for sending. Cancelling can no longer guarantee that
    /// the worker will not execute.
    Starting,
    /// The guest accepted Start; only Exit can establish whether exec succeeded.
    Started,
    /// Cleanup is being observed; progress reports do not extend its deadline.
    Closing,
    Closed,
    /// A host operation failed. A retained final Exit can still confirm cleanup.
    Failed,
}

/// Each report replaces the previous one rather than appending to an output
/// history. Clones share the captured bytes, which outlive the session driver.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    phase: SessionPhase,
    exit: Option<Arc<SessionExit>>,
    error: Option<Arc<BrokerSessionError>>,
}

impl SessionSnapshot {
    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    /// The latest validated guest report. A report retained during failure is
    /// diagnostic evidence and does not turn Failed into Closed.
    pub fn exit(&self) -> Option<&SessionExit> {
        self.exit.as_deref()
    }

    /// Retains the first host-side failure even if cleanup later completes.
    /// A Closed session can therefore have both an error and a final Exit.
    pub fn error(&self) -> Option<&BrokerSessionError> {
        self.error.as_deref()
    }

    /// Keeps the report after dropping the snapshot, without copying output.
    pub fn into_exit(self) -> Option<Arc<SessionExit>> {
        self.exit
    }
}

/// Values allowed to change between Prepare and Start. The account, command
/// and projection paths remain fixed by the original Prepare request.
#[derive(Default)]
pub struct SessionStartOptions {
    pub wayland_display: Option<String>,
    pub x11_display: Option<u16>,
    pub terminal_size: Option<TerminalSize>,
}

/// Dropping the handle requests cleanup but does not wait for it. The host
/// observes cleanup for at most 15 seconds; the guardian remains responsible
/// for reaping after the host stops waiting.
///
/// Cleanup acknowledgements and progress cannot extend this budget. A complete
/// reply must be processed before the deadline to complete the operation.
/// Failure leaves Failed, but a nonblocking read of the bytes already queued
/// can retain Exit alongside the error. It never waits for a missing frame tail
/// or lets later writes extend this final read. Invalid framing stops the read.
#[must_use]
pub struct BrokerSession {
    preparation: PreparationId,
    observation: WorkerObservation,
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<SessionSnapshot>,
}

type Reply<T> = oneshot::Sender<Result<T, BrokerSessionError>>;

enum Command {
    Recheck {
        deadline: Instant,
        reply: Reply<WorkerObservation>,
    },
    Start {
        deadline: Instant,
        options: SessionStartOptions,
        evidence: ProjectionObservations,
        reply: Reply<Option<OwnedFd>>,
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
                let _ = reply.send(Err(error));
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
        let deadline = deadline.min(Instant::now() + PREPARE_TIMEOUT);
        if Instant::now() >= deadline {
            return Err(BrokerSessionError::Deadline);
        }
        let frame = connection
            .readiness()
            .prepare(2, request)
            .map_err(|error| BrokerSessionError::InvalidRequest(error.to_string()))?;
        let maximum = connection
            .readiness()
            .limits()
            .get()
            .max_control_frame_bytes() as usize
            - lasper_guest_protocol::CONTROL_FRAME_HEADER_BYTES;
        let payload = wire::encode(maximum, |output| frame.request().encode_payload(output))?;
        let length = payload.len();
        let header = ControlFrameHeader::new(
            lasper_guest_protocol::MessageKind::PrepareRequest,
            frame.request_id(),
            length as u32,
            0,
        )
        .map_err(wire::encoding)?;
        if Instant::now() >= deadline {
            return Err(BrokerSessionError::Deadline);
        }
        let (commands, receiver) = mpsc::channel(1);
        let (state, snapshots) = watch::channel(SessionSnapshot {
            phase: SessionPhase::Prepared,
            exit: None,
            error: None,
        });
        let (ready, received) = oneshot::channel();
        tokio::spawn(driver::run(
            connection, header, payload, receiver, state, ready, deadline,
        ));
        let (preparation, observation) = timeout_at(deadline, received)
            .await
            .map_err(|_| BrokerSessionError::Deadline)?
            .map_err(|_| BrokerSessionError::DriverStopped)??;
        if Instant::now() >= deadline {
            return Err(BrokerSessionError::Deadline);
        }
        Ok(Self {
            preparation,
            observation,
            commands,
            state: snapshots,
        })
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

    /// The caller must approve any changed display evidence before Start. A
    /// cancelled recheck leaves the caller's previous observation unchanged.
    /// Queueing and identity verification share a five-second budget.
    pub async fn recheck(&mut self) -> Result<&WorkerObservation, BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Recheck {
            deadline: Instant::now() + START_TIMEOUT,
            reply,
        })
        .await?;
        self.observation = received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)??;
        Ok(&self.observation)
    }

    /// Start has a five-second budget, including queueing and identity checks.
    /// Changed projection evidence leaves the worker prepared for the caller
    /// to review. Success means Start was accepted, not that exec succeeded.
    ///
    /// Before Start is queued for sending, an observed cancellation preserves
    /// preparation. Cancellation racing with that point requests Close, since
    /// the worker may already execute. A lost acknowledgement is reported as
    /// StartOutcomeUnknown and is never retried.
    pub async fn start(
        &mut self,
        options: SessionStartOptions,
    ) -> Result<Option<OwnedFd>, BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        let (delivered, delivery) = oneshot::channel();
        self.send(Command::Start {
            deadline: Instant::now() + START_TIMEOUT,
            options,
            evidence: self.observation.reply().projections(),
            reply,
            delivered: delivery,
        })
        .await?;
        let terminal = received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)??;
        // The driver needs to distinguish a delivered result from a reply
        // abandoned in the oneshot slot. Do not yield between this and return.
        let _ = delivered.send(());
        Ok(terminal)
    }

    pub async fn cancel(&mut self) -> Result<(), BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Cancel {
            deadline: Instant::now() + OPERATION_TIMEOUT,
            reply,
        })
        .await?;
        received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)?
    }

    /// Ok confirms the cleanup request, not that descendants have been reaped.
    /// Continue observing snapshots for the final Exit; without it, the
    /// original cleanup deadline ends observation with Failed and Deadline.
    pub async fn close(&mut self) -> Result<(), BrokerSessionError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Close {
            deadline: Instant::now() + OPERATION_TIMEOUT,
            reply,
        })
        .await?;
        received
            .await
            .map_err(|_| BrokerSessionError::DriverStopped)?
    }

    async fn send(&self, command: Command) -> Result<(), BrokerSessionError> {
        timeout_at(command.deadline(), self.commands.send(command))
            .await
            .map_err(|_| BrokerSessionError::Deadline)?
            .map_err(|_| BrokerSessionError::DriverStopped)
    }
}
