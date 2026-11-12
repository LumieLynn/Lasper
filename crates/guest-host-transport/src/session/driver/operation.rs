use super::control::{unexpected_reply, CleanupRequest, ExpectedReply, Outbound, RequestPurpose};
use super::Owner;
use crate::session::{wire, BrokerSessionError, Reply, SessionPhase, OPERATION_TIMEOUT};
use crate::WorkerObservation;
use lasper_guest_protocol::{
    ControlFrameHeader, MessageKind, PreparationControlKind, PreparationId, ProjectionObservations,
};
use std::future::pending;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::time::Instant;

pub(super) struct StartPlan {
    pub payload: Box<[u8]>,
    pub evidence: ProjectionObservations,
    pub reply: Reply<Option<OwnedFd>>,
    pub delivered: oneshot::Receiver<()>,
}

pub(super) enum CheckPurpose {
    Recheck(Reply<WorkerObservation>),
    Start(StartPlan),
}

pub(super) enum Operation {
    Prepare {
        reply: Reply<(PreparationId, WorkerObservation)>,
        deadline: Instant,
    },
    Check {
        purpose: CheckPurpose,
        deadline: Instant,
    },
    ReadyToStart {
        start: StartPlan,
        deadline: Instant,
    },
    Starting {
        reply: Reply<Option<OwnedFd>>,
        delivered: oneshot::Receiver<()>,
        deadline: Instant,
    },
    Delivering {
        received: oneshot::Receiver<()>,
        deadline: Instant,
    },
}

pub(super) enum OperationEvent {
    PrepareAbandoned,
    StartWithdrawn,
    StartAbandoned,
    Delivered(bool),
}

impl Operation {
    pub fn deadline(&self) -> Instant {
        match self {
            Self::Prepare { deadline, .. }
            | Self::Check { deadline, .. }
            | Self::ReadyToStart { deadline, .. }
            | Self::Starting { deadline, .. }
            | Self::Delivering { deadline, .. } => *deadline,
        }
    }

    pub async fn advance(&mut self) -> OperationEvent {
        match self {
            Self::Prepare { reply, .. } => {
                reply.closed().await;
                OperationEvent::PrepareAbandoned
            }
            Self::Starting { reply, .. } => {
                reply.closed().await;
                OperationEvent::StartAbandoned
            }
            Self::Delivering { received, .. } => OperationEvent::Delivered(received.await.is_ok()),
            Self::ReadyToStart { start, .. } => {
                start.reply.closed().await;
                OperationEvent::StartWithdrawn
            }
            // Finish a started observation even if its caller is gone. A later
            // operation must not consume the abandoned challenge's reply.
            Self::Check { .. } => pending().await,
        }
    }

    pub fn reject(self, error: BrokerSessionError) -> BrokerSessionError {
        let error = if matches!(&self, Self::Starting { .. })
            && !matches!(
                &error,
                BrokerSessionError::Rejected(_) | BrokerSessionError::StartInterrupted(_)
            ) {
            BrokerSessionError::StartOutcomeUnknown(Arc::new(error))
        } else {
            error
        };
        match self {
            Self::Prepare { reply, .. } => {
                let _ = reply.send(Err(error.clone()));
            }
            Self::Check {
                purpose: CheckPurpose::Recheck(reply),
                ..
            } => {
                let _ = reply.send(Err(error.clone()));
            }
            Self::Check {
                purpose: CheckPurpose::Start(start),
                ..
            }
            | Self::ReadyToStart { start, .. } => {
                let _ = start.reply.send(Err(error.clone()));
            }
            Self::Starting { reply, .. } => {
                let _ = reply.send(Err(error.clone()));
            }
            Self::Delivering { .. } => {}
        }
        error
    }
}

impl Owner<'_> {
    pub(super) fn observed(
        &mut self,
        observed: io::Result<WorkerObservation>,
    ) -> Result<(), BrokerSessionError> {
        // Identity errors make Start unsafe, but the control stream may still
        // carry the guardian's account of worker exit and descendant cleanup.
        let observed = match observed.and_then(|observed| {
            self.connection.recheck()?;
            Ok(observed)
        }) {
            Ok(observed) => observed,
            Err(error) => return self.invalidate_preparation(error.into()),
        };
        self.check_deadlines()?;
        if self.phase() != SessionPhase::Prepared {
            return Ok(());
        }
        match self.operation.take() {
            Some(Operation::Prepare { reply, .. }) => {
                if reply
                    .send(Ok((self.token.preparation_id(), observed)))
                    .is_err()
                {
                    self.abandon_start()?;
                }
            }
            Some(Operation::Check {
                purpose: CheckPurpose::Recheck(reply),
                ..
            }) => {
                let _ = reply.send(Ok(observed));
            }
            Some(Operation::Check {
                purpose: CheckPurpose::Start(start),
                deadline,
            }) => {
                if start.reply.is_closed() {
                    return Ok(());
                }
                if observed.reply().projections() != start.evidence {
                    let _ = start.reply.send(Err(BrokerSessionError::ProjectionChanged));
                } else {
                    self.operation = Some(Operation::ReadyToStart { start, deadline });
                }
            }
            operation => {
                self.operation = operation;
                return Err(unexpected_reply());
            }
        }
        Ok(())
    }

    pub(super) fn submit_start(&mut self) -> Result<(), BrokerSessionError> {
        if !self.control_available() {
            return Ok(());
        }
        let (start, deadline) = match self.operation.take() {
            Some(Operation::ReadyToStart { start, deadline }) => (start, deadline),
            other => {
                self.operation = other;
                return Ok(());
            }
        };
        if start.reply.is_closed() {
            return Ok(());
        }
        if let Err(error) = self.worker.verify() {
            self.operation = Some(Operation::ReadyToStart { start, deadline });
            return self.invalidate_preparation(error.into());
        }
        // Keep may have used more request IDs while identity was pending.
        // Assign Start's ID at submission so the guest sees increasing IDs.
        let header = self.request_id().and_then(|id| {
            ControlFrameHeader::new(MessageKind::Start, id, start.payload.len() as u32, 0)
                .map_err(wire::encoding)
        });
        let header = match header {
            Ok(header) => header,
            Err(error) => {
                let _ = start.reply.send(Err(error.clone()));
                return Err(error);
            }
        };
        let expected = ExpectedReply::Start(header);
        self.operation = Some(Operation::Starting {
            reply: start.reply,
            delivered: start.delivered,
            deadline,
        });
        if let Err(error) = self.exchange(
            Outbound {
                header,
                payload: start.payload,
            },
            expected,
            RequestPurpose::Start,
            deadline.min(self.lease_deadline),
        ) {
            // Queueing failed before the Start submission boundary.
            if let Some(Operation::Starting { reply, .. }) = self.operation.take() {
                let _ = reply.send(Err(error.clone()));
            }
            return Err(error);
        }
        self.set_phase(SessionPhase::Starting);
        Ok(())
    }

    pub(super) fn invalidate_preparation(
        &mut self,
        error: BrokerSessionError,
    ) -> Result<(), BrokerSessionError> {
        if let Some(operation) = self.operation.take() {
            operation.reject(error.clone());
        }
        self.state.send_modify(|state| {
            state.error.get_or_insert_with(|| Arc::new(error));
        });
        self.begin_cleanup(CleanupRequest {
            kind: PreparationControlKind::Close,
            deadline: Instant::now() + OPERATION_TIMEOUT,
            reply: None,
        })
    }

    pub(super) fn operation_event(
        &mut self,
        event: OperationEvent,
    ) -> Result<(), BrokerSessionError> {
        match event {
            OperationEvent::PrepareAbandoned | OperationEvent::StartAbandoned => {
                self.abandon_start()?
            }
            OperationEvent::StartWithdrawn => {
                self.operation.take();
            }
            OperationEvent::Delivered(delivered) => {
                self.operation.take();
                if !delivered {
                    self.abandon_start()?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn abandon_start(&mut self) -> Result<(), BrokerSessionError> {
        self.begin_cleanup(CleanupRequest {
            kind: PreparationControlKind::Close,
            deadline: Instant::now() + OPERATION_TIMEOUT,
            reply: None,
        })
    }
}
