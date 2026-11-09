use crate::transport::{GuestControlStream, IoBudget};
use core::task::Poll;
use lasper_guest_protocol::{
    EnforcedControlLimits, PreparationControl, PreparationControlFrame, PreparationId,
    PrepareReadyFrame, PrepareRequestFrame, StartAcceptedFrame, StartFrame, MAX_PREPARED_LEASE_MS,
};

use super::process::GuardianWorker;
use super::{
    CleanupProgress, CleanupReason, ExecOutcome, GuardianError, SessionAction, SessionConnection,
    SessionLifecycle, SessionPhase, StartDecision, WorkerBootstrap, WorkerObservation,
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
pub struct SessionGuardian<'a, 'control> {
    session: SessionLifecycle,
    worker: GuardianWorker<'a>,
    connection: &'control GuestControlStream,
    limits: EnforcedControlLimits,
    pending: Option<super::ready::PendingReply<'control>>,
    ready_frame: Option<PrepareReadyFrame>,
    completed_ready: Option<PrepareReadyFrame>,
    execution_deadline_ms: Option<u64>,
}

impl<'a, 'control> SessionGuardian<'a, 'control> {
    pub(super) unsafe fn spawn(
        connection: &'control GuestControlStream,
        limits: EnforcedControlLimits,
        request: &'a PrepareRequestFrame<'a>,
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
            connection,
            limits,
            pending: None,
            ready_frame: None,
            completed_ready: None,
            execution_deadline_ms: None,
        })
    }

    pub const fn pid(&self) -> i32 {
        self.worker.pid()
    }

    pub const fn phase(&self) -> SessionPhase {
        self.session.phase()
    }

    pub fn next_deadline_ms(&self) -> Option<u64> {
        let lifecycle = match self.phase() {
            SessionPhase::Preparing
            | SessionPhase::Prepared
            | SessionPhase::DeliveringReady
            | SessionPhase::AwaitingStart => Some(self.session.deadline_ms()),
            SessionPhase::Starting | SessionPhase::Running => self.execution_deadline_ms,
            SessionPhase::Closing | SessionPhase::CleanupPending => {
                self.worker.cleanup_deadline_ms()
            }
            SessionPhase::Closed => None,
        };
        [
            lifecycle,
            self.pending.as_ref().map(|reply| reply.deadline_ms()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(super) fn accepts_commands(&self) -> bool {
        matches!(
            self.phase(),
            SessionPhase::AwaitingStart | SessionPhase::Starting | SessionPhase::Running
        )
    }
    pub(super) fn remaining_lease_ms(&self, now_ms: u64) -> u32 {
        self.session
            .deadline_ms()
            .saturating_sub(now_ms)
            .min(u32::MAX as u64) as u32
    }
    pub(super) const fn completed_ready(&self) -> Option<PrepareReadyFrame> {
        self.completed_ready
    }
    pub(super) fn has_reply(&self) -> bool {
        self.pending.is_some()
    }

    /// The event loop supplies its monotonic clock. It must continue polling
    /// CleanupPending sessions; a timeout or closed client does not reap a child.
    pub fn poll(&mut self, now_ms: u64) -> Result<GuardianObservation, GuardianError> {
        if matches!(self.phase(), SessionPhase::Starting | SessionPhase::Running)
            && self
                .execution_deadline_ms
                .is_some_and(|deadline| now_ms >= deadline)
        {
            self.fail(now_ms, CleanupReason::ExecutionTimedOut);
        }
        if let Some(action) = self.session.tick(now_ms) {
            self.perform(action, now_ms);
        }
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

    /// Incrementally sends Ready on this session's connection. Pending keeps
    /// the FD copies and byte offset here, leaving poll free to drive cleanup.
    pub fn deliver_ready(
        &mut self,
        now_ms: u64,
        budget: &mut IoBudget,
    ) -> Result<Poll<PrepareReadyFrame>, GuardianError> {
        self.poll(now_ms)?;
        if self.phase() == SessionPhase::AwaitingStart {
            return Err(GuardianError::ReadyAlreadyTaken);
        }
        if self.phase() == SessionPhase::Prepared {
            self.queue_ready(now_ms)?;
        }
        if self.phase() != SessionPhase::DeliveringReady {
            return Err(GuardianError::NotPrepared);
        }
        match self.flush(now_ms, budget)? {
            Poll::Ready(super::ready::ReplyKind::Ready) => Ok(Poll::Ready(
                self.ready_frame.ok_or(GuardianError::NotPrepared)?,
            )),
            _ => Ok(Poll::Pending),
        }
    }

    pub(super) fn queue_ready(&mut self, now_ms: u64) -> Result<(), GuardianError> {
        if self.pending.is_some() {
            return Err(GuardianError::ReplyPending);
        }
        self.session
            .begin_ready_delivery()
            .map_err(GuardianError::Lifecycle)?;
        let remaining = self.remaining_lease_ms(now_ms);
        let result = self.worker.ready(remaining, |frame, descriptors| {
            let frame = super::ready::Payload::ready(frame)?;
            self.pending = Some(super::ready::PendingReply::new(
                self.connection,
                self.limits,
                super::ready::ReplyKind::Ready,
                now_ms,
                frame,
                descriptors,
            )?);
            Ok(())
        });
        match result {
            Ok(frame) => {
                self.ready_frame = Some(frame);
                Ok(())
            }
            Err(error) => {
                self.fail(now_ms, CleanupReason::ReadyDeliveryFailed);
                Err(error)
            }
        }
    }

    pub(super) fn queue_reply(
        &mut self,
        kind: super::ready::ReplyKind,
        now_ms: u64,
        frame: (
            lasper_guest_protocol::ControlFrameHeader,
            super::ready::Payload,
        ),
    ) -> Result<(), GuardianError> {
        if self.pending.is_some() {
            return Err(GuardianError::ReplyPending);
        }
        self.pending = Some(super::ready::PendingReply::new(
            self.connection,
            self.limits,
            kind,
            now_ms,
            frame,
            &[],
        )?);
        Ok(())
    }

    pub(super) fn flush(
        &mut self,
        now_ms: u64,
        budget: &mut IoBudget,
    ) -> Result<Poll<super::ready::ReplyKind>, GuardianError> {
        let Some(reply) = self.pending.as_mut() else {
            return Ok(Poll::Pending);
        };
        match reply.advance(now_ms, budget) {
            Ok(Poll::Ready(kind)) => {
                self.pending.take();
                if kind == super::ready::ReplyKind::Ready {
                    self.session
                        .ready_delivered()
                        .map_err(GuardianError::Lifecycle)?;
                    self.completed_ready = self.ready_frame;
                }
                Ok(Poll::Ready(kind))
            }
            Ok(Poll::Pending) => Ok(Poll::Pending),
            Err(error) => {
                let kind = reply.kind();
                self.pending.take();
                self.fail(
                    now_ms,
                    if kind == super::ready::ReplyKind::Ready {
                        CleanupReason::ReadyDeliveryFailed
                    } else {
                        CleanupReason::ClientClosed
                    },
                );
                Err(error)
            }
        }
    }

    pub(super) fn fail(&mut self, now_ms: u64, reason: CleanupReason) {
        let action = self.session.fail(reason);
        self.perform(action, now_ms);
    }

    pub(super) fn close_output(&mut self) {
        self.worker.close_output();
    }

    pub(super) fn force_cleanup(&mut self, reason: CleanupReason) {
        self.session.fail(reason);
        if self
            .pending
            .as_ref()
            .is_some_and(|reply| reply.kind() == super::ready::ReplyKind::Ready)
        {
            self.pending.take();
        }
        self.worker.force_cleanup();
    }

    /// Handles control EOF or the broker-liveness pipe closing. The event loop
    /// keeps this owner alive until poll reports Closed, including Pending cleanup.
    pub fn connection_closed(&mut self, now_ms: u64, connection: SessionConnection) {
        let action = self.session.connection_closed(connection);
        self.perform(action, now_ms);
        if connection == SessionConnection::Client {
            self.pending.take();
        }
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
        if self.worker.bounded() {
            self.execution_deadline_ms = Some(now_ms.saturating_add(60_000));
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
            if matches!(
                self.pending.as_ref().map(|reply| reply.kind()),
                Some(super::ready::ReplyKind::Ready)
            ) {
                self.pending.take();
            }
            self.worker.begin_cleanup(now_ms);
        }
    }
}
