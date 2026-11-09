use lasper_guest_protocol::{
    BrokerId, PreparationControlFrame, PreparationControlKind, PreparationId, StartFrame,
    StartRequest, MAX_PREPARED_LEASE_MS,
};

pub(super) const WORKER_SETUP_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Preparing,
    Prepared,
    DeliveringReady,
    AwaitingStart,
    Starting,
    Running,
    Closing,
    CleanupPending,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupReason {
    PreparationExpired,
    WorkerPreparationFailed,
    WorkerObservationFailed,
    ClockFailed,
    ReadyDeliveryFailed,
    Cancelled,
    LeaseExpired,
    ClientClosed,
    BrokerExited,
    WorkerExitedBeforeStart,
    WorkerExited,
    ExecFailed { errno: i32 },
    StartOutcomeUnknown,
    SupervisorStopped,
    ExecutionTimedOut,
}

/// Exec evidence is separate from worker termination. `Unknown` does not
/// invalidate an exit code or signal obtained from wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecOutcome {
    Started,
    Failed { errno: i32 },
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionConnection {
    Client,
    Broker,
    Supervisor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAction {
    Prepared { deadline_ms: u64 },
    LeaseRenewed { deadline_ms: u64 },
    Recheck,
    BeginCleanup(CleanupReason),
    Running,
    CleanupPending(CleanupReason),
    Closed(CleanupReason),
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleError {
    BrokerMismatch,
    PreparationMismatch,
    InvalidLease,
    ClockOverflow,
    InvalidTransition { phase: SessionPhase },
}

/// Start carries an inline path buffer, so its request stays out of the small
/// results used by lease, control, and cleanup events.
#[derive(Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum StartDecision {
    Accepted(StartRequest),
    BeginCleanup(CleanupReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Preparing,
    Prepared,
    DeliveringReady,
    AwaitingStart,
    Starting,
    Running,
    Closing(CleanupReason),
    CleanupPending(CleanupReason),
    Closed(CleanupReason),
}

/// Serializes lease, Start, Cancel, and Close decisions for one prepared worker.
/// The guardian calls this from its event loop before performing each returned action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLifecycle {
    broker_id: BrokerId,
    preparation_id: PreparationId,
    deadline_ms: u64,
    lease_ms: u32,
    state: State,
}

impl SessionLifecycle {
    pub fn new(
        broker_id: BrokerId,
        preparation_id: PreparationId,
        now_ms: u64,
        lease_ms: u32,
    ) -> Result<Self, SessionLifecycleError> {
        if lease_ms == 0 || lease_ms > MAX_PREPARED_LEASE_MS {
            return Err(SessionLifecycleError::InvalidLease);
        }
        let deadline_ms = now_ms
            .checked_add(u64::from(lease_ms))
            .ok_or(SessionLifecycleError::ClockOverflow)?;
        Ok(Self {
            broker_id,
            preparation_id,
            deadline_ms,
            lease_ms,
            state: State::Prepared,
        })
    }

    pub const fn phase(&self) -> SessionPhase {
        match self.state {
            State::Preparing => SessionPhase::Preparing,
            State::Prepared => SessionPhase::Prepared,
            State::DeliveringReady => SessionPhase::DeliveringReady,
            State::AwaitingStart => SessionPhase::AwaitingStart,
            State::Starting => SessionPhase::Starting,
            State::Running => SessionPhase::Running,
            State::Closing(_) => SessionPhase::Closing,
            State::CleanupPending(_) => SessionPhase::CleanupPending,
            State::Closed(_) => SessionPhase::Closed,
        }
    }

    pub const fn cleanup_reason(&self) -> Option<CleanupReason> {
        match self.state {
            State::Closing(reason) | State::CleanupPending(reason) | State::Closed(reason) => {
                Some(reason)
            }
            _ => None,
        }
    }

    pub fn preparing(
        broker_id: BrokerId,
        preparation_id: PreparationId,
        now_ms: u64,
        lease_ms: u32,
    ) -> Result<Self, SessionLifecycleError> {
        let mut lifecycle = Self::new(broker_id, preparation_id, now_ms, lease_ms)?;
        lifecycle.deadline_ms = now_ms
            .checked_add(WORKER_SETUP_TIMEOUT_MS)
            .ok_or(SessionLifecycleError::ClockOverflow)?;
        lifecycle.state = State::Preparing;
        Ok(lifecycle)
    }

    pub fn worker_prepared(&mut self, now_ms: u64) -> Result<SessionAction, SessionLifecycleError> {
        if let Some(action) = self.tick(now_ms) {
            return Ok(action);
        }
        if self.state != State::Preparing {
            return Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            });
        }
        let deadline_ms = now_ms
            .checked_add(u64::from(self.lease_ms))
            .ok_or(SessionLifecycleError::ClockOverflow)?;
        self.deadline_ms = deadline_ms;
        self.state = State::Prepared;
        Ok(SessionAction::Prepared { deadline_ms })
    }

    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub(super) fn begin_ready_delivery(&mut self) -> Result<(), SessionLifecycleError> {
        if self.state != State::Prepared {
            return Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            });
        }
        self.state = State::DeliveringReady;
        Ok(())
    }

    pub(super) fn ready_delivered(&mut self) -> Result<(), SessionLifecycleError> {
        if self.state != State::DeliveringReady {
            return Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            });
        }
        self.state = State::AwaitingStart;
        Ok(())
    }

    pub(super) fn fail(&mut self, reason: CleanupReason) -> SessionAction {
        if self.cleanup_reason().is_some() {
            SessionAction::Noop
        } else {
            self.begin_cleanup(reason)
        }
    }

    /// Expires only an unstarted preparation. Once Start wins, the lease no
    /// longer controls the lifetime of the running guest process.
    pub fn tick(&mut self, now_ms: u64) -> Option<SessionAction> {
        self.expiration(now_ms)
            .map(|reason| self.begin_cleanup(reason))
    }

    fn expiration(&self, now_ms: u64) -> Option<CleanupReason> {
        match self.state {
            State::Preparing if now_ms >= self.deadline_ms => {
                Some(CleanupReason::PreparationExpired)
            }
            State::Prepared | State::DeliveringReady | State::AwaitingStart
                if now_ms >= self.deadline_ms =>
            {
                Some(CleanupReason::LeaseExpired)
            }
            _ => None,
        }
    }

    /// Returns the accepted request for delivery to the assigned worker.
    /// An expired preparation enters cleanup without releasing the request.
    pub fn accept_start(
        &mut self,
        now_ms: u64,
        frame: StartFrame,
    ) -> Result<StartDecision, SessionLifecycleError> {
        self.validate_token(
            frame.request().broker_id(),
            frame.request().preparation_id(),
        )?;
        if let Some(reason) = self.expiration(now_ms) {
            self.begin_cleanup(reason);
            return Ok(StartDecision::BeginCleanup(reason));
        }
        if self.state != State::AwaitingStart {
            return Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            });
        }

        self.state = State::Starting;
        Ok(StartDecision::Accepted(frame.into_request()))
    }

    /// Applies one control frame in the guardian's serialized event loop.
    pub fn control(
        &mut self,
        now_ms: u64,
        frame: PreparationControlFrame,
    ) -> Result<SessionAction, SessionLifecycleError> {
        let control = frame.control();
        self.validate_token(control.broker_id(), control.preparation_id())?;
        if let Some(action) = self.tick(now_ms) {
            return Ok(action);
        }

        match (self.state, frame.kind()) {
            (State::AwaitingStart, PreparationControlKind::KeepPrepared) => {
                let deadline_ms = now_ms
                    .checked_add(u64::from(MAX_PREPARED_LEASE_MS))
                    .ok_or(SessionLifecycleError::ClockOverflow)?;
                self.deadline_ms = deadline_ms;
                Ok(SessionAction::LeaseRenewed { deadline_ms })
            }
            (State::AwaitingStart, PreparationControlKind::Recheck) => Ok(SessionAction::Recheck),
            (
                State::Preparing | State::Prepared | State::DeliveringReady | State::AwaitingStart,
                PreparationControlKind::Cancel,
            ) => Ok(self.begin_cleanup(CleanupReason::Cancelled)),
            (
                State::Preparing
                | State::Prepared
                | State::DeliveringReady
                | State::AwaitingStart
                | State::Starting
                | State::Running,
                PreparationControlKind::Close,
            ) => Ok(self.begin_cleanup(CleanupReason::ClientClosed)),
            (
                State::Closing(_) | State::CleanupPending(_) | State::Closed(_),
                PreparationControlKind::Cancel | PreparationControlKind::Close,
            ) => Ok(SessionAction::Noop),
            _ => Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            }),
        }
    }

    /// Reports the result observed on the worker's CLOEXEC error pipe together
    /// with its wait state. Missing or ambiguous evidence must use `Unknown`.
    pub fn exec_outcome(
        &mut self,
        outcome: ExecOutcome,
    ) -> Result<SessionAction, SessionLifecycleError> {
        match (self.state, outcome) {
            (State::Starting, ExecOutcome::Started) => {
                self.state = State::Running;
                Ok(SessionAction::Running)
            }
            (State::Starting, ExecOutcome::Failed { errno }) => {
                Ok(self.begin_cleanup(CleanupReason::ExecFailed { errno }))
            }
            (State::Starting, ExecOutcome::Unknown) => {
                Ok(self.begin_cleanup(CleanupReason::StartOutcomeUnknown))
            }
            (State::Closing(_) | State::CleanupPending(_) | State::Closed(_), _) => {
                Ok(SessionAction::Noop)
            }
            _ => Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            }),
        }
    }

    pub fn worker_exited(&mut self) -> SessionAction {
        match self.state {
            State::Preparing | State::Prepared | State::DeliveringReady | State::AwaitingStart => {
                self.begin_cleanup(CleanupReason::WorkerExitedBeforeStart)
            }
            State::Starting | State::Running => self.begin_cleanup(CleanupReason::WorkerExited),
            State::Closing(_) | State::CleanupPending(_) | State::Closed(_) => SessionAction::Noop,
        }
    }

    pub fn worker_failed(&mut self) -> SessionAction {
        match self.state {
            State::Preparing | State::Prepared | State::DeliveringReady | State::AwaitingStart => {
                self.begin_cleanup(CleanupReason::WorkerPreparationFailed)
            }
            State::Starting => self.begin_cleanup(CleanupReason::StartOutcomeUnknown),
            State::Running => self.begin_cleanup(CleanupReason::WorkerObservationFailed),
            State::Closing(_) | State::CleanupPending(_) | State::Closed(_) => SessionAction::Noop,
        }
    }

    pub fn connection_closed(&mut self, connection: SessionConnection) -> SessionAction {
        match self.state {
            State::Preparing
            | State::Prepared
            | State::DeliveringReady
            | State::AwaitingStart
            | State::Starting
            | State::Running => self.begin_cleanup(match connection {
                SessionConnection::Broker => CleanupReason::BrokerExited,
                SessionConnection::Client => CleanupReason::ClientClosed,
                SessionConnection::Supervisor => CleanupReason::SupervisorStopped,
            }),
            State::Closing(_) | State::CleanupPending(_) | State::Closed(_) => SessionAction::Noop,
        }
    }

    /// Records whether the owner completed the cleanup attempt.
    pub fn cleanup_result(
        &mut self,
        complete: bool,
    ) -> Result<SessionAction, SessionLifecycleError> {
        let reason = match self.state {
            State::Closing(reason) | State::CleanupPending(reason) => reason,
            _ => {
                return Err(SessionLifecycleError::InvalidTransition {
                    phase: self.phase(),
                })
            }
        };
        if complete {
            self.state = State::Closed(reason);
            Ok(SessionAction::Closed(reason))
        } else {
            self.state = State::CleanupPending(reason);
            Ok(SessionAction::CleanupPending(reason))
        }
    }

    pub fn retry_cleanup(&mut self) -> Result<SessionAction, SessionLifecycleError> {
        match self.state {
            State::CleanupPending(reason) => Ok(self.begin_cleanup(reason)),
            _ => Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            }),
        }
    }

    fn validate_token(
        &self,
        broker_id: BrokerId,
        preparation_id: PreparationId,
    ) -> Result<(), SessionLifecycleError> {
        if broker_id != self.broker_id {
            return Err(SessionLifecycleError::BrokerMismatch);
        }
        if preparation_id != self.preparation_id {
            return Err(SessionLifecycleError::PreparationMismatch);
        }
        Ok(())
    }

    fn begin_cleanup(&mut self, reason: CleanupReason) -> SessionAction {
        self.state = State::Closing(reason);
        SessionAction::BeginCleanup(reason)
    }
}

#[cfg(test)]
#[path = "../../tests/session/state.rs"]
mod tests;
