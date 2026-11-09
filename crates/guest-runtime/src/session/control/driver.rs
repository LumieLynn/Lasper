use core::task::Poll;
use lasper_guest_protocol::{
    ErrorFrame, ExitFrame, ExitReport, GuestErrorCode, GuestOperation, PreparationId,
    PrepareRequestFrame, CONTROL_FRAME_HEADER_BYTES, MAX_CAPTURED_OUTPUT_BYTES,
};

use crate::linux::uapi::{
    errno::EINTR,
    poll::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLRDHUP},
};
use crate::transport::{IoBudget, IO_BYTES_PER_TURN};

use super::super::error::SystemResultExt;
use super::super::linux::{self, PollFd};
use super::super::ready::{Payload, ReplyKind};
use super::super::{
    CleanupReason, GuardianContext, GuardianError, GuardianObservation, SessionConnection,
    SessionGuardian, SessionPhase, SupervisorStatus,
};
use super::capture::Capture;
use super::input::{Input, InputProgress};

const MAX_EXIT_PAYLOAD_BYTES: usize = 4096 + 4 * MAX_CAPTURED_OUTPUT_BYTES.div_ceil(3);
// A queued reply must finish before the cleanup report. Each pass can send at
// most these two frames; a blocked socket ends the attempt rather than waiting.
const CLOCK_FAILURE_FLUSH_TURNS: usize =
    2 * (MAX_EXIT_PAYLOAD_BYTES + CONTROL_FRAME_HEADER_BYTES).div_ceil(IO_BYTES_PER_TURN);

// Only wire progress lives here. Worker phases and cleanup decisions remain in
// SessionGuardian, even after the control connection can no longer be used.
struct ControlLoop<'context, 'request> {
    context: &'context GuardianContext,
    request: &'request PrepareRequestFrame<'request>,
    preparation: PreparationId,
    input: Option<Input<'context>>,
    last_request_id: u64,
    reported: Option<ExitReport>,
    prepare_rejected: bool,
    failure: Option<GuardianError>,
}

pub(super) fn run<'request>(
    context: &GuardianContext,
    request: &'request PrepareRequestFrame<'request>,
    preparation: PreparationId,
    guardian: &mut SessionGuardian<'_, '_>,
    capture: Option<Capture>,
) -> Result<(), GuardianError> {
    ControlLoop {
        context,
        request,
        preparation,
        input: None,
        last_request_id: request.request_id(),
        reported: None,
        prepare_rejected: false,
        failure: None,
    }
    .drive(guardian, capture, linux::monotonic_ms)
}

impl ControlLoop<'_, '_> {
    fn drive(
        mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        mut capture: Option<Capture>,
        mut clock: impl FnMut() -> Result<u64, i32>,
    ) -> Result<(), GuardianError> {
        let mut last_now = self.context.hello_completed_at_ms();
        loop {
            let now = match clock().context("read guardian session clock") {
                Ok(now) => now,
                Err(error) => {
                    return self.finish_without_clock(guardian, capture.as_mut(), last_now, error);
                }
            };
            last_now = now;
            let supervisor_alive = self.supervise(guardian, now);
            if !self.context.connection().can_continue() {
                self.input.take();
                guardian.connection_closed(now, SessionConnection::Client);
            }
            let observation = match guardian.poll(now) {
                Ok(observation) => observation,
                Err(error) => {
                    guardian.fail(now, CleanupReason::WorkerObservationFailed);
                    self.fail(guardian, now, error);
                    continue;
                }
            };
            if self
                .input_deadline(guardian)
                .is_some_and(|deadline| now >= deadline)
            {
                self.fail(guardian, now, GuardianError::ControlExpired);
            }
            let mut budget = IoBudget::new();
            if let Some(capture) = capture.as_mut() {
                if observation.phase == SessionPhase::Closed {
                    capture.after_cleanup(now);
                }
                if let Err(error) = capture.drain(guardian, now, &mut budget) {
                    capture.abandon(guardian);
                    guardian.fail(now, CleanupReason::WorkerObservationFailed);
                    self.fail(guardian, now, error);
                }
            }
            let output_finished = capture.as_ref().is_none_or(Capture::finished);
            if self.context.connection().can_continue() {
                match self.advance(guardian, observation, capture.as_ref(), now, &mut budget) {
                    Ok(true) => return self.result(),
                    Ok(false) => {}
                    Err(error) => self.fail(guardian, now, error),
                }
            }
            if guardian.phase() == SessionPhase::Closed
                && output_finished
                && (!self.context.connection().can_continue()
                    || (self.prepare_rejected && !guardian.has_reply()))
            {
                return self.result();
            }
            let deadline = [
                guardian.next_deadline_ms(),
                self.input_deadline(guardian),
                capture.as_ref().and_then(Capture::deadline_ms),
            ]
            .into_iter()
            .flatten()
            .min();
            if let Err(error) = self.wait(guardian, supervisor_alive, now, deadline) {
                self.fail(guardian, now, error);
            }
        }
    }

    fn input_deadline(&self, guardian: &SessionGuardian<'_, '_>) -> Option<u64> {
        if guardian.accepts_commands() {
            self.input.as_ref().and_then(Input::deadline_ms)
        } else {
            // Keep a partial reader alive through cleanup and Exit delivery.
            // Dropping it now would poison the connection; its deadline no longer applies.
            None
        }
    }

    fn finish_without_clock(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        mut capture: Option<&mut Capture>,
        last_now: u64,
        error: GuardianError,
    ) -> Result<(), GuardianError> {
        // No grace or output deadline can be measured now. Keep the connection
        // usable for a bounded report attempt, but do not read another command.
        self.failure.get_or_insert(error);
        guardian.force_cleanup(CleanupReason::ClockFailed);
        if let Some(capture) = capture.as_deref_mut() {
            capture.abandon(guardian);
        }
        let mut first_observation = true;
        loop {
            match guardian.poll(last_now) {
                Ok(observation) => {
                    let closed = observation.phase == SessionPhase::Closed;
                    if first_observation || closed {
                        self.report_without_clock(
                            guardian,
                            observation,
                            capture.as_deref(),
                            last_now,
                        );
                        first_observation = false;
                    }
                    if closed {
                        self.close_connection(guardian, last_now);
                        return self.result();
                    }
                }
                Err(_) => guardian.force_cleanup(CleanupReason::ClockFailed),
            }
            let [status, pidfd, _, _] = guardian.poll_descriptors();
            let _ = linux::poll(&mut [PollFd::new(status), PollFd::new(pidfd)], Some(10));
        }
    }

    fn report_without_clock(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        observation: GuardianObservation,
        capture: Option<&Capture>,
        last_now: u64,
    ) {
        if !self.context.connection().can_continue() {
            return;
        }
        // last_now only satisfies the existing reply API. The finite I/O budget,
        // not a frozen absolute deadline, bounds this emergency flush.
        for _ in 0..CLOCK_FAILURE_FLUSH_TURNS {
            let mut budget = IoBudget::new();
            match self.advance(guardian, observation, capture, last_now, &mut budget) {
                Ok(true) => return,
                Ok(false) if !guardian.has_reply() => return,
                Ok(false) if !budget.available() => continue,
                _ => break,
            }
        }
        self.close_connection(guardian, last_now);
    }

    fn supervise(&mut self, guardian: &mut SessionGuardian<'_, '_>, now: u64) -> bool {
        match self.context.supervisor_status() {
            Ok(SupervisorStatus::Alive) => return true,
            Ok(SupervisorStatus::BrokerDisconnected) => {
                guardian.connection_closed(now, SessionConnection::Broker)
            }
            Ok(SupervisorStatus::Stopped(_)) => {
                guardian.connection_closed(now, SessionConnection::Supervisor)
            }
            Err(error) => {
                guardian.connection_closed(now, SessionConnection::Supervisor);
                self.fail(guardian, now, error);
            }
        }
        false
    }

    fn fail(&mut self, guardian: &mut SessionGuardian<'_, '_>, now: u64, error: GuardianError) {
        self.failure.get_or_insert(error);
        self.close_connection(guardian, now);
    }

    fn close_connection(&mut self, guardian: &mut SessionGuardian<'_, '_>, now: u64) {
        self.context.connection().poison();
        self.input.take();
        guardian.connection_closed(now, SessionConnection::Client);
    }

    fn result(&self) -> Result<(), GuardianError> {
        self.failure.map_or(Ok(()), Err)
    }

    fn advance(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        observation: GuardianObservation,
        capture: Option<&Capture>,
        now: u64,
        budget: &mut IoBudget,
    ) -> Result<bool, GuardianError> {
        // Close is dispatched after this send loop, so its reply is only queued
        // in that turn. The next turn polls the cleanup state, sends the queued
        // reply first, then tries to send the report within the same I/O budget.
        loop {
            if !guardian.has_reply() {
                self.queue_reply(guardian, observation, capture, now)?;
            }
            match guardian.flush(now, budget)? {
                Poll::Ready(ReplyKind::Exit { final_report: true }) => return Ok(true),
                Poll::Ready(_) if observation.cleanup_reason.is_some() => continue,
                _ => break,
            }
        }
        if !guardian.has_reply() && !self.prepare_rejected && guardian.accepts_commands() {
            let reader = match self.input.take() {
                Some(reader) => reader,
                None => Input::new(self.context.connection())?,
            };
            match reader.advance(now, budget)? {
                InputProgress::Pending(reader) => self.input = Some(reader),
                InputProgress::Complete(frame) => super::dispatch::dispatch(
                    guardian,
                    self.request,
                    self.preparation,
                    &mut self.last_request_id,
                    frame,
                    now,
                )?,
            }
        }
        Ok(false)
    }

    fn queue_reply(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        observation: GuardianObservation,
        capture: Option<&Capture>,
        now: u64,
    ) -> Result<(), GuardianError> {
        if observation.phase == SessionPhase::Prepared {
            return guardian.queue_ready(now);
        }
        let Some(reason) = observation.cleanup_reason else {
            return Ok(());
        };
        if guardian.completed_ready().is_none() {
            return self.reject_prepare(guardian, reason, observation, now);
        }
        let Some(report) = super::report::report(observation)? else {
            return Ok(());
        };
        let final_report = report.is_final() && capture.is_none_or(Capture::finished);
        if self.reported == Some(report) || (report.is_final() && !final_report) {
            return Ok(());
        }
        let output = capture.map(Capture::view).transpose()?;
        let exit = ExitFrame::new(self.request, self.preparation, report, output)
            .map_err(|_| GuardianError::ReplyEncoding)?;
        let capacity = (self
            .context
            .negotiated_limits()
            .get()
            .max_control_frame_bytes() as usize)
            .min(MAX_EXIT_PAYLOAD_BYTES);
        let payload = Payload::encode(capacity, |bytes| {
            exit.encode_parts(bytes)
                .map_err(GuardianError::SessionFrame)
        })?;
        guardian.queue_reply(ReplyKind::Exit { final_report }, now, payload)?;
        self.reported = Some(report);
        Ok(())
    }

    fn reject_prepare(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        reason: CleanupReason,
        observation: GuardianObservation,
        now: u64,
    ) -> Result<(), GuardianError> {
        if self.prepare_rejected {
            return Ok(());
        }
        let (code, errno) = match (reason, self.failure) {
            (CleanupReason::ClockFailed, Some(GuardianError::System { errno, .. })) => {
                (GuestErrorCode::System, Some(errno))
            }
            _ => super::report::prepare_error(reason, observation.worker),
        };
        let error = ErrorFrame::new(
            self.request.request_id(),
            self.request.broker_id(),
            None,
            GuestOperation::Prepare,
            code,
            errno,
        )
        .map_err(|_| GuardianError::ReplyEncoding)?;
        let payload = Payload::encode(4096, |bytes| {
            error
                .encode_parts(bytes)
                .map_err(GuardianError::SessionFrame)
        })?;
        guardian.queue_reply(ReplyKind::Command, now, payload)?;
        self.prepare_rejected = true;
        Ok(())
    }

    fn wait(
        &self,
        guardian: &SessionGuardian<'_, '_>,
        supervisor_alive: bool,
        now: u64,
        deadline: Option<u64>,
    ) -> Result<(), GuardianError> {
        if deadline.is_some_and(|deadline| deadline <= now) {
            return Ok(());
        }
        let [control, broker, signals] = self.context.poll_descriptors();
        let worker = guardian.poll_descriptors();
        let connected = self.context.connection().can_continue();
        let mut descriptors = [
            PollFd::new(if connected { control } else { -1 }),
            PollFd::new(if supervisor_alive { broker } else { -1 }),
            PollFd::new(if supervisor_alive { signals } else { -1 }),
            PollFd::new(worker[0]),
            PollFd::new(worker[1]),
            PollFd::new(worker[2]),
            PollFd::new(worker[3]),
        ];
        descriptors[0].events = (if guardian.has_reply() {
            POLLOUT
        } else if guardian.accepts_commands() {
            POLLIN
        } else {
            0
        }) | POLLRDHUP;
        match linux::poll(
            &mut descriptors,
            deadline.map(|deadline| deadline.saturating_sub(now)),
        ) {
            Err(EINTR) => Ok(()),
            Err(errno) => Err(GuardianError::System {
                operation: "wait for guardian session",
                errno,
            }),
            Ok(_) if descriptors[0].returned & (POLLHUP | POLLERR | POLLRDHUP) != 0 => {
                self.context.connection().poison();
                Ok(())
            }
            Ok(_) => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/control/clock.rs"]
mod clock_tests;
