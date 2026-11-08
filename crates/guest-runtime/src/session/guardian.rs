use lasper_guest_protocol::{
    PreparationControl, PreparationControlFrame, PreparationId, PrepareRequestFrame,
    StartAcceptedFrame, StartFrame, MAX_PREPARED_LEASE_MS,
};

use super::process::GuardianWorker;
use super::{
    CleanupProgress, CleanupReason, ExecOutcome, GuardianError, SessionAction, SessionConnection,
    SessionLifecycle, SessionPhase, StartDecision, WorkerBootstrap, WorkerObservation, WorkerReady,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianObservation {
    pub phase: SessionPhase,
    pub cleanup_reason: Option<CleanupReason>,
    pub worker: WorkerObservation,
}

/// Owns the protocol decision and the assigned child through cleanup. All
/// events are applied by one guardian loop; callers do not synchronize a second
/// process registry with the preparation state.
pub struct SessionGuardian<'a> {
    session: SessionLifecycle,
    worker: GuardianWorker<'a>,
}

impl<'a> SessionGuardian<'a> {
    pub(super) unsafe fn spawn(
        request: PrepareRequestFrame<'a>,
        preparation: PreparationId,
        now_ms: u64,
        entry: fn(WorkerBootstrap<'_>) -> !,
    ) -> Result<Self, GuardianError> {
        let lifecycle = SessionLifecycle::preparing(
            request.broker_id(),
            preparation,
            now_ms,
            MAX_PREPARED_LEASE_MS,
        )
        .map_err(GuardianError::Lifecycle)?;
        // SAFETY: The caller guarantees exclusive child ownership and a fork-safe entry.
        let worker = unsafe { GuardianWorker::spawn(request, preparation, entry) }?;
        Ok(Self {
            session: lifecycle,
            worker,
        })
    }

    pub const fn pid(&self) -> i32 {
        self.worker.pid()
    }

    pub const fn phase(&self) -> SessionPhase {
        self.session.phase()
    }

    /// Used alongside `poll_descriptors` to wait without periodic busy polling.
    /// Running sessions have no preparation timer; cleanup has its own retries.
    pub const fn next_deadline_ms(&self) -> Option<u64> {
        match self.phase() {
            SessionPhase::Preparing
            | SessionPhase::Prepared
            | SessionPhase::DeliveringReady
            | SessionPhase::AwaitingStart => Some(self.session.deadline_ms()),
            SessionPhase::Closing | SessionPhase::CleanupPending => {
                self.worker.cleanup_deadline_ms()
            }
            _ => None,
        }
    }

    /// The event loop supplies its monotonic clock. It must continue polling
    /// CleanupPending sessions; a timeout or closed client does not reap a child.
    pub fn poll(&mut self, now_ms: u64) -> Result<GuardianObservation, GuardianError> {
        if let Some(action) = self.session.tick(now_ms) {
            self.perform(action, now_ms);
        }
        // Ready can be dropped outside poll. Apply its cleanup decision before
        // observing worker exit, retaining the original reason in the lifecycle.
        if self.session.cleanup_reason().is_some() && self.phase() != SessionPhase::Closed {
            self.worker.begin_cleanup(now_ms);
        }
        let worker = self.worker.poll(now_ms);
        if self.phase() == SessionPhase::Preparing && worker.prepared {
            let action = self
                .session
                .worker_prepared(now_ms)
                .map_err(GuardianError::Lifecycle)?;
            self.perform(action, now_ms);
        }
        // Preserve an explicit exec failure. Otherwise, a known exit does not
        // require an earlier Running observation to complete the session.
        let action = match (self.phase(), worker.exec) {
            (SessionPhase::Starting, Some(ExecOutcome::Failed { errno })) => self
                .session
                .exec_outcome(ExecOutcome::Failed { errno })
                .map_err(GuardianError::Lifecycle)?,
            _ if worker.failure.is_some() => self.session.worker_failed(),
            _ if worker.exit.is_some() => self.session.worker_exited(),
            _ if worker.error.is_some() => self.session.worker_failed(),
            (SessionPhase::Starting, Some(outcome)) => self
                .session
                .exec_outcome(outcome)
                .map_err(GuardianError::Lifecycle)?,
            _ => SessionAction::Noop,
        };
        self.perform(action, now_ms);
        match (self.phase(), worker.cleanup) {
            (SessionPhase::Closing | SessionPhase::CleanupPending, CleanupProgress::Complete) => {
                self.session
                    .cleanup_result(true)
                    .map_err(GuardianError::Lifecycle)?;
            }
            (SessionPhase::Closing, CleanupProgress::Pending { .. }) => {
                self.session
                    .cleanup_result(false)
                    .map_err(GuardianError::Lifecycle)?;
            }
            _ => {}
        }
        Ok(GuardianObservation {
            phase: self.phase(),
            cleanup_reason: self.session.cleanup_reason(),
            worker: self.worker.observation(),
        })
    }

    /// Polls worker setup and expiry before taking the one Ready delivery.
    /// Call `deliver` on the returned owner to send it. Start is disabled until
    /// that delivery succeeds; dropping it requests cleanup on the next poll.
    pub fn take_ready(&mut self, now_ms: u64) -> Result<WorkerReady<'_>, GuardianError> {
        self.poll(now_ms)?;
        if self.phase() == SessionPhase::AwaitingStart {
            return Err(GuardianError::ReadyAlreadyTaken);
        }
        if self.phase() != SessionPhase::Prepared {
            return Err(GuardianError::NotPrepared);
        }
        let remaining = self
            .session
            .deadline_ms()
            .saturating_sub(now_ms)
            .min(u32::MAX as u64) as u32;
        let transition = self
            .session
            .begin_ready_delivery()
            .map_err(GuardianError::Lifecycle)?;
        let resources = self.worker.take_ready(remaining)?;
        Ok(WorkerReady::new(resources, transition))
    }

    /// Handles control EOF or the broker-liveness pipe closing. The event loop
    /// keeps this owner alive until poll reports Closed, including Pending cleanup.
    pub fn connection_closed(&mut self, now_ms: u64, connection: SessionConnection) {
        let action = self.session.connection_closed(connection);
        self.perform(action, now_ms);
    }

    /// Acknowledges the one Start accepted for this assignment. This says
    /// nothing about exec success; poll observations retain that separate result.
    pub fn start(
        &mut self,
        now_ms: u64,
        frame: StartFrame,
    ) -> Result<StartAcceptedFrame, GuardianError> {
        self.poll(now_ms)?;
        self.worker.validate_start(frame.request())?;
        let request_id = frame.request_id();
        let token = PreparationControl::new(
            frame.request().broker_id(),
            frame.request().preparation_id(),
        );
        let decision = self
            .session
            .accept_start(now_ms, frame)
            .map_err(GuardianError::Lifecycle)?;
        let start = match decision {
            StartDecision::Accepted(start) => start,
            StartDecision::BeginCleanup(reason) => {
                self.perform(SessionAction::BeginCleanup(reason), now_ms);
                return Err(GuardianError::NotPrepared);
            }
        };
        if let Err(error) = self.worker.start(&start) {
            let action = self
                .session
                .exec_outcome(ExecOutcome::Unknown)
                .map_err(GuardianError::Lifecycle)?;
            self.perform(action, now_ms);
            return Err(error);
        }
        Ok(StartAcceptedFrame::new(request_id, token))
    }

    pub fn control(
        &mut self,
        now_ms: u64,
        frame: PreparationControlFrame,
    ) -> Result<SessionAction, GuardianError> {
        self.poll(now_ms)?;
        let action = self
            .session
            .control(now_ms, frame)
            .map_err(GuardianError::Lifecycle)?;
        self.perform(action, now_ms);
        Ok(action)
    }

    pub fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<usize, GuardianError> {
        self.worker.read_stdout(buffer)
    }

    pub fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<usize, GuardianError> {
        self.worker.read_stderr(buffer)
    }

    pub fn poll_descriptors(&self) -> [i32; 4] {
        self.worker.poll_descriptors()
    }

    fn perform(&mut self, action: SessionAction, now_ms: u64) {
        if let SessionAction::BeginCleanup(_) = action {
            self.worker.begin_cleanup(now_ms);
        }
    }
}
