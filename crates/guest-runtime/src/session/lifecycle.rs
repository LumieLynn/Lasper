use lasper_guest_protocol::{
    BrokerId, PreparationControlFrame, PreparationControlKind, PreparationId, StartFrame,
    StartRequest, MAX_PREPARED_LEASE_MS,
};

const WORKER_SETUP_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Preparing,
    Prepared,
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
    Cancelled,
    LeaseExpired,
    ClientClosed,
    BrokerExited,
    WorkerExitedBeforeStart,
    WorkerExited,
    ExecFailed { errno: i32 },
    StartOutcomeUnknown,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAction {
    Prepared { deadline_ms: u64 },
    LeaseRenewed { deadline_ms: u64 },
    Recheck,
    StartAccepted,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Preparing,
    Prepared,
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

    /// Expires only an unstarted preparation. Once Start wins, the lease no
    /// longer controls the lifetime of the running guest process.
    pub fn tick(&mut self, now_ms: u64) -> Option<SessionAction> {
        match self.state {
            State::Preparing if now_ms >= self.deadline_ms => {
                Some(self.begin_cleanup(CleanupReason::PreparationExpired))
            }
            State::Prepared if now_ms >= self.deadline_ms => {
                Some(self.begin_cleanup(CleanupReason::LeaseExpired))
            }
            _ => None,
        }
    }

    /// Commits the one-way Prepared -> Starting transition.
    ///
    /// The frame is borrowed deliberately: the owner keeps the validated
    /// request and passes it to the worker only after this decision wins.
    pub fn accept_start(
        &mut self,
        now_ms: u64,
        frame: &StartFrame,
    ) -> Result<SessionAction, SessionLifecycleError> {
        self.validate_token(
            frame.request().broker_id(),
            frame.request().preparation_id(),
        )?;
        if let Some(action) = self.tick(now_ms) {
            return Ok(action);
        }
        if self.state != State::Prepared {
            return Err(SessionLifecycleError::InvalidTransition {
                phase: self.phase(),
            });
        }

        self.state = State::Starting;
        Ok(SessionAction::StartAccepted)
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
            (State::Prepared, PreparationControlKind::KeepPrepared) => {
                let deadline_ms = now_ms
                    .checked_add(u64::from(MAX_PREPARED_LEASE_MS))
                    .ok_or(SessionLifecycleError::ClockOverflow)?;
                self.deadline_ms = deadline_ms;
                Ok(SessionAction::LeaseRenewed { deadline_ms })
            }
            (State::Prepared, PreparationControlKind::Recheck) => Ok(SessionAction::Recheck),
            (State::Preparing | State::Prepared, PreparationControlKind::Cancel) => {
                Ok(self.begin_cleanup(CleanupReason::Cancelled))
            }
            (
                State::Preparing | State::Prepared | State::Starting | State::Running,
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
            State::Preparing | State::Prepared => {
                self.begin_cleanup(CleanupReason::WorkerExitedBeforeStart)
            }
            State::Starting | State::Running => self.begin_cleanup(CleanupReason::WorkerExited),
            State::Closing(_) | State::CleanupPending(_) | State::Closed(_) => SessionAction::Noop,
        }
    }

    pub fn worker_failed(&mut self) -> SessionAction {
        match self.state {
            State::Preparing | State::Prepared => {
                self.begin_cleanup(CleanupReason::WorkerPreparationFailed)
            }
            State::Starting => self.begin_cleanup(CleanupReason::StartOutcomeUnknown),
            State::Running => self.begin_cleanup(CleanupReason::WorkerObservationFailed),
            State::Closing(_) | State::CleanupPending(_) | State::Closed(_) => SessionAction::Noop,
        }
    }

    pub fn connection_closed(&mut self, connection: SessionConnection) -> SessionAction {
        match self.state {
            State::Preparing | State::Prepared | State::Starting | State::Running => self
                .begin_cleanup(match connection {
                    SessionConnection::Broker => CleanupReason::BrokerExited,
                    SessionConnection::Client => CleanupReason::ClientClosed,
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

/// Owns the lifecycle decision and the one Start request accepted by it.
/// A guardian keeps this object beside the worker resources it must clean up.
#[derive(Debug, Eq, PartialEq)]
pub struct PreparedSession {
    lifecycle: SessionLifecycle,
    start: Option<StartFrame>,
}

impl PreparedSession {
    pub const fn new(lifecycle: SessionLifecycle) -> Self {
        Self {
            lifecycle,
            start: None,
        }
    }

    pub const fn phase(&self) -> SessionPhase {
        self.lifecycle.phase()
    }

    pub const fn cleanup_reason(&self) -> Option<CleanupReason> {
        self.lifecycle.cleanup_reason()
    }

    pub const fn deadline_ms(&self) -> u64 {
        self.lifecycle.deadline_ms()
    }

    pub fn tick(&mut self, now_ms: u64) -> Option<SessionAction> {
        self.lifecycle.tick(now_ms)
    }

    pub fn accept_start(
        &mut self,
        now_ms: u64,
        frame: StartFrame,
    ) -> Result<SessionAction, SessionLifecycleError> {
        let action = self.lifecycle.accept_start(now_ms, &frame)?;
        if action == SessionAction::StartAccepted {
            self.start = Some(frame);
        }
        Ok(action)
    }

    /// Returns the exact request accepted by Start. It can be consumed once;
    /// the caller must not construct a replacement request after acceptance.
    pub fn take_start(&mut self) -> Option<StartRequest> {
        self.start.take().map(StartFrame::into_request)
    }

    pub fn control(
        &mut self,
        now_ms: u64,
        frame: PreparationControlFrame,
    ) -> Result<SessionAction, SessionLifecycleError> {
        self.lifecycle.control(now_ms, frame)
    }

    pub fn exec_outcome(
        &mut self,
        outcome: ExecOutcome,
    ) -> Result<SessionAction, SessionLifecycleError> {
        self.lifecycle.exec_outcome(outcome)
    }

    pub fn worker_exited(&mut self) -> SessionAction {
        self.lifecycle.worker_exited()
    }

    pub fn worker_prepared(&mut self, now_ms: u64) -> Result<SessionAction, SessionLifecycleError> {
        self.lifecycle.worker_prepared(now_ms)
    }

    pub fn worker_failed(&mut self) -> SessionAction {
        self.lifecycle.worker_failed()
    }

    pub fn connection_closed(&mut self, connection: SessionConnection) -> SessionAction {
        self.lifecycle.connection_closed(connection)
    }

    pub fn cleanup_result(
        &mut self,
        complete: bool,
    ) -> Result<SessionAction, SessionLifecycleError> {
        self.lifecycle.cleanup_result(complete)
    }

    pub fn retry_cleanup(&mut self) -> Result<SessionAction, SessionLifecycleError> {
        self.lifecycle.retry_cleanup()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lasper_guest_protocol::{
        CommandArgv, ExecutionKind, GuestProcess, PrepareRequest, PrepareRequestFrame,
        ProjectionRequests, StartRequest, TerminalEnvironment, TerminalSize,
    };

    const BROKER: BrokerId = BrokerId::new([1; 16]);
    const PREPARATION: PreparationId = PreparationId::new([2; 16]);

    fn request() -> PrepareRequestFrame<'static> {
        PrepareRequestFrame::new(
            10,
            BROKER,
            PrepareRequest::new(
                "alice",
                ExecutionKind::Interactive,
                GuestProcess::Command(CommandArgv::new(&["/usr/bin/editor"]).unwrap()),
                Some(TerminalEnvironment::new("xterm", None, None).unwrap()),
                Some(TerminalSize::new(80, 24).unwrap()),
                ProjectionRequests::default(),
            )
            .unwrap(),
        )
    }

    fn lifecycle() -> SessionLifecycle {
        SessionLifecycle::new(BROKER, PREPARATION, 100, 10_000).unwrap()
    }

    #[test]
    fn setup_deadline_is_separate_from_the_prepared_lease() {
        let mut session = SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
        assert_eq!(session.phase(), SessionPhase::Preparing);
        assert_eq!(session.deadline_ms(), 5_100);
        assert_eq!(
            session.control(101, control(PreparationControlKind::KeepPrepared)),
            Err(SessionLifecycleError::InvalidTransition {
                phase: SessionPhase::Preparing
            })
        );
        assert_eq!(
            session.worker_prepared(5_099),
            Ok(SessionAction::Prepared {
                deadline_ms: 35_099
            })
        );
        assert_eq!(session.phase(), SessionPhase::Prepared);

        let mut expired = SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
        assert_eq!(
            expired.worker_prepared(5_100),
            Ok(SessionAction::BeginCleanup(
                CleanupReason::PreparationExpired
            ))
        );
        assert_eq!(expired.phase(), SessionPhase::Closing);
    }

    #[test]
    fn client_and_broker_loss_cancel_both_preparing_and_running_workers() {
        for connection in [SessionConnection::Client, SessionConnection::Broker] {
            let reason = match connection {
                SessionConnection::Client => CleanupReason::ClientClosed,
                SessionConnection::Broker => CleanupReason::BrokerExited,
            };
            let mut preparing =
                SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
            assert_eq!(
                preparing.connection_closed(connection),
                SessionAction::BeginCleanup(reason)
            );
            assert_eq!(preparing.connection_closed(connection), SessionAction::Noop);
            let mut running = lifecycle();
            running.accept_start(101, &start_frame()).unwrap();
            running.exec_outcome(ExecOutcome::Started).unwrap();
            assert_eq!(
                running.connection_closed(connection),
                SessionAction::BeginCleanup(reason)
            );
        }
    }

    fn start_frame() -> StartFrame {
        let request = request();
        let start = StartRequest::new(
            &request,
            PREPARATION,
            None,
            None,
            Some(TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap();
        StartFrame::new(11, start)
    }

    fn control(kind: PreparationControlKind) -> PreparationControlFrame {
        PreparationControlFrame::new(
            12,
            kind,
            lasper_guest_protocol::PreparationControl::new(BROKER, PREPARATION),
        )
    }

    #[test]
    fn start_is_one_shot_and_ends_the_preparation_lease() {
        let mut session = lifecycle();
        assert_eq!(session.phase(), SessionPhase::Prepared);

        let frame = start_frame();
        let action = session.accept_start(101, &frame).unwrap();
        assert_eq!(action, SessionAction::StartAccepted);
        assert_eq!(session.phase(), SessionPhase::Starting);
        assert_eq!(session.tick(u64::MAX), None);
        assert_eq!(
            session.accept_start(102, &frame),
            Err(SessionLifecycleError::InvalidTransition {
                phase: SessionPhase::Starting,
            }),
        );
    }

    #[test]
    fn expiry_wins_at_the_deadline_before_a_queued_start() {
        let mut session = lifecycle();
        assert_eq!(session.deadline_ms(), 10_100);
        assert_eq!(
            session.accept_start(10_100, &start_frame()),
            Ok(SessionAction::BeginCleanup(CleanupReason::LeaseExpired)),
        );
        assert_eq!(session.phase(), SessionPhase::Closing);
    }

    #[test]
    fn serialized_start_and_cancel_have_one_winner() {
        let mut start_wins = lifecycle();
        let frame = start_frame();
        start_wins.accept_start(101, &frame).unwrap();
        assert_eq!(
            start_wins.control(102, control(PreparationControlKind::Cancel)),
            Err(SessionLifecycleError::InvalidTransition {
                phase: SessionPhase::Starting,
            }),
        );

        let mut cancel_wins = lifecycle();
        assert_eq!(
            cancel_wins.control(101, control(PreparationControlKind::Cancel)),
            Ok(SessionAction::BeginCleanup(CleanupReason::Cancelled)),
        );
        assert_eq!(
            cancel_wins.accept_start(102, &start_frame()),
            Err(SessionLifecycleError::InvalidTransition {
                phase: SessionPhase::Closing,
            }),
        );
    }

    #[test]
    fn keep_prepared_renews_only_a_live_preparation() {
        let mut session = lifecycle();
        assert_eq!(
            session.control(1_000, control(PreparationControlKind::KeepPrepared)),
            Ok(SessionAction::LeaseRenewed {
                deadline_ms: 46_000,
            }),
        );
        assert_eq!(session.deadline_ms(), 46_000);
        assert_eq!(
            session.control(46_000, control(PreparationControlKind::KeepPrepared)),
            Ok(SessionAction::BeginCleanup(CleanupReason::LeaseExpired)),
        );
    }

    #[test]
    fn start_result_preserves_failed_and_unknown_outcomes() {
        let mut failed = lifecycle();
        let frame = start_frame();
        failed.accept_start(101, &frame).unwrap();
        assert_eq!(
            failed.exec_outcome(ExecOutcome::Failed { errno: 2 }),
            Ok(SessionAction::BeginCleanup(CleanupReason::ExecFailed {
                errno: 2
            })),
        );

        let mut unknown = lifecycle();
        let frame = start_frame();
        unknown.accept_start(101, &frame).unwrap();
        assert_eq!(
            unknown.exec_outcome(ExecOutcome::Unknown),
            Ok(SessionAction::BeginCleanup(
                CleanupReason::StartOutcomeUnknown
            )),
        );
    }

    #[test]
    fn worker_exit_does_not_require_a_running_observation() {
        let mut session = lifecycle();
        session.accept_start(101, &start_frame()).unwrap();
        assert_eq!(
            session.worker_exited(),
            SessionAction::BeginCleanup(CleanupReason::WorkerExited)
        );
        assert_eq!(
            session.exec_outcome(ExecOutcome::Unknown),
            Ok(SessionAction::Noop)
        );
        assert_eq!(
            session.cleanup_result(true),
            Ok(SessionAction::Closed(CleanupReason::WorkerExited))
        );
    }

    #[test]
    fn close_and_cleanup_pending_keep_one_cleanup_owner() {
        let mut session = lifecycle();
        let frame = start_frame();
        session.accept_start(101, &frame).unwrap();
        session.exec_outcome(ExecOutcome::Started).unwrap();
        assert_eq!(
            session.control(102, control(PreparationControlKind::Close)),
            Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
        );
        assert_eq!(
            session.cleanup_result(false),
            Ok(SessionAction::CleanupPending(CleanupReason::ClientClosed)),
        );
        assert_eq!(session.phase(), SessionPhase::CleanupPending);
        assert_eq!(
            session.retry_cleanup(),
            Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
        );
        assert_eq!(
            session.cleanup_result(true),
            Ok(SessionAction::Closed(CleanupReason::ClientClosed)),
        );
        assert_eq!(session.phase(), SessionPhase::Closed);
        assert_eq!(
            session.control(200, control(PreparationControlKind::Close)),
            Ok(SessionAction::Noop),
        );
    }

    #[test]
    fn close_cancels_an_unstarted_preparation() {
        let mut session = lifecycle();
        assert_eq!(
            session.control(101, control(PreparationControlKind::Close)),
            Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
        );
        assert_eq!(session.phase(), SessionPhase::Closing);
    }

    #[test]
    fn commands_must_match_the_preparation_owner() {
        let mut session = lifecycle();
        let wrong = PreparationControlFrame::new(
            12,
            PreparationControlKind::Cancel,
            lasper_guest_protocol::PreparationControl::new(BROKER, PreparationId::new([3; 16])),
        );
        assert_eq!(
            session.control(101, wrong),
            Err(SessionLifecycleError::PreparationMismatch),
        );
        assert_eq!(session.phase(), SessionPhase::Prepared);
    }

    #[test]
    fn prepared_session_rejects_a_zero_or_overlong_lease() {
        assert_eq!(
            SessionLifecycle::new(BROKER, PREPARATION, 0, 0),
            Err(SessionLifecycleError::InvalidLease),
        );
        assert_eq!(
            SessionLifecycle::new(BROKER, PREPARATION, 0, MAX_PREPARED_LEASE_MS + 1),
            Err(SessionLifecycleError::InvalidLease),
        );
    }

    #[test]
    fn prepared_session_keeps_the_accepted_start_request_once() {
        let mut session = PreparedSession::new(lifecycle());
        let frame = start_frame();
        assert_eq!(
            session.accept_start(101, frame),
            Ok(SessionAction::StartAccepted),
        );
        let request = session.take_start().expect("accepted Start is retained");
        assert_eq!(
            request.terminal_size(),
            Some(TerminalSize::new(80, 24).unwrap())
        );
        assert!(session.take_start().is_none());
    }
}
