use lasper_guest_protocol::{
    PreparationControl, PreparationControlFrame, PreparationId, PrepareReadyFrame,
    PrepareRequestFrame, StartAcceptedFrame, StartFrame, MAX_PREPARED_LEASE_MS,
};

use super::process::GuardianWorker;
use super::{
    CleanupProgress, CleanupReason, ExecOutcome, GuardianError, PreparedSession, SessionAction,
    SessionConnection, SessionLifecycle, SessionPhase, WorkerBootstrap, WorkerDescriptors,
    WorkerObservation,
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
    session: PreparedSession,
    worker: GuardianWorker<'a>,
}

impl<'a> SessionGuardian<'a> {
    /// Starts worker setup with a five-second deadline. The preparation lease
    /// begins only after that same worker reports readiness.
    ///
    /// # Safety
    ///
    /// The caller must be a dedicated single-threaded guardian, with exclusive
    /// ownership of child creation/reaping. Entry is trusted helper code and
    /// must use fork-safe operations before exec/exit, without unwinding.
    pub unsafe fn spawn(
        request: PrepareRequestFrame<'a>,
        preparation: PreparationId,
        now_ms: u64,
        entry: fn(WorkerBootstrap<'a>) -> !,
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
            session: PreparedSession::new(lifecycle),
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
            SessionPhase::Preparing | SessionPhase::Prepared => Some(self.session.deadline_ms()),
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

    pub fn ready_frame(&mut self, now_ms: u64) -> Result<PrepareReadyFrame, GuardianError> {
        self.poll(now_ms)?;
        if self.phase() != SessionPhase::Prepared {
            return Err(GuardianError::NotPrepared);
        }
        let remaining = self
            .session
            .deadline_ms()
            .saturating_sub(now_ms)
            .min(u32::MAX as u64) as u32;
        self.worker.ready_frame(remaining)
    }

    pub fn descriptors(&self) -> Result<WorkerDescriptors<'_>, GuardianError> {
        if self.phase() != SessionPhase::Prepared {
            return Err(GuardianError::NotPrepared);
        }
        self.worker.descriptors()
    }

    pub fn release_exported_descriptors(&mut self) {
        self.worker.release_exported_descriptors();
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
        let action = self
            .session
            .accept_start(now_ms, frame)
            .map_err(GuardianError::Lifecycle)?;
        if action != SessionAction::StartAccepted {
            self.perform(action, now_ms);
            return Err(GuardianError::NotPrepared);
        }
        let start = self
            .session
            .take_start()
            .expect("accepted Start retains its request");
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
