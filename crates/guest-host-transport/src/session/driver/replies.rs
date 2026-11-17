use super::control::{unexpected_reply, ReplyValue, RequestPurpose};
use super::operation::Operation;
use super::worker::Worker;
use super::{renewal, Owner};
use crate::session::{BrokerSessionError, SessionPhase, StartOutcome};
use crate::ReceivedControlFrame;
use lasper_guest_protocol::{ControlReplyStatus, ExitReport, MessageKind};
use tokio::time::Instant;

impl Owner<'_> {
    pub(super) fn decode_reply(
        &mut self,
        expected: super::control::ExpectedReply,
        frame: &ReceivedControlFrame,
    ) -> Result<ReplyValue, BrokerSessionError> {
        let decoded = expected.decode(
            &self.hello.protocol(),
            &self.request,
            self.token.preparation_id(),
            frame,
        );
        let reply = match &decoded {
            Ok(ReplyValue::StartAccepted) => lasper_guest_protocol::WireReply::StartAccepted,
            Ok(ReplyValue::Control(ControlReplyStatus::Prepared { .. })) => {
                lasper_guest_protocol::WireReply::Prepared
            }
            Ok(ReplyValue::Control(ControlReplyStatus::CleanupRequested)) => {
                lasper_guest_protocol::WireReply::CleanupRequested
            }
            Err(BrokerSessionError::Rejected(_)) => lasper_guest_protocol::WireReply::Rejected,
            Err(_) => return decoded,
        };
        self.wire_state
            .reply(frame.header().request_id(), expected.operation(), reply)
            .map_err(crate::session::wire::sequence)?;
        decoded
    }

    pub(super) fn receive(
        &mut self,
        frame: ReceivedControlFrame,
    ) -> Result<(), BrokerSessionError> {
        if let Err(error) = self.check_deadlines() {
            self.fail(error, Some(frame));
            return Ok(());
        }
        self.frame_deadline = None;
        if frame.header().kind() == MessageKind::Exit {
            return self.exit(frame);
        }
        if self.state.borrow().report.is_some() {
            let expected = if self
                .retired
                .is_some_and(|reply| reply.header().request_id() == frame.header().request_id())
            {
                self.retired.take()
            } else {
                self.pending.take().map(|pending| pending.expected)
            }
            .ok_or_else(unexpected_reply)?;
            match self.decode_reply(expected, &frame) {
                Ok(ReplyValue::StartAccepted) => self
                    .state
                    .send_modify(|s| s.submission = crate::StartSubmission::Accepted),
                Err(BrokerSessionError::Rejected(_))
                    if matches!(expected, super::control::ExpectedReply::Start(_)) =>
                {
                    self.state
                        .send_modify(|s| s.submission = crate::StartSubmission::Rejected)
                }
                Ok(_) | Err(BrokerSessionError::Rejected(_)) => {}
                Err(error) => return Err(error),
            }
            return Ok(());
        }
        if let Some(expected) = self.retired {
            if expected.header().request_id() == frame.header().request_id() {
                match self.decode_reply(expected, &frame) {
                    Ok(ReplyValue::StartAccepted) => self
                        .state
                        .send_modify(|s| s.submission = crate::StartSubmission::Accepted),
                    Err(BrokerSessionError::Rejected(_))
                        if matches!(expected, super::control::ExpectedReply::Start(_)) =>
                    {
                        self.state
                            .send_modify(|s| s.submission = crate::StartSubmission::Rejected)
                    }
                    Ok(_) | Err(BrokerSessionError::Rejected(_)) => {}
                    Err(error) => return Err(error),
                }
                self.retired = None;
                return Ok(());
            }
        }
        let pending = self.pending.take().ok_or_else(unexpected_reply)?;
        let value = match self.decode_reply(pending.expected, &frame) {
            Ok(value) => value,
            Err(error) => {
                if matches!(pending.purpose, RequestPurpose::Start)
                    && matches!(error, BrokerSessionError::Rejected(_))
                {
                    self.state.send_modify(|s| {
                        s.submission = crate::StartSubmission::Rejected;
                        s.phase = SessionPhase::Closing;
                    });
                    if let Some(operation) = self.operation.take() {
                        operation.reject(error);
                    }
                    return self.begin_cleanup(super::control::CleanupRequest {
                        kind: lasper_guest_protocol::PreparationControlKind::Close,
                        deadline: Instant::now() + self.timeouts.cleanup_observation,
                        reply: None,
                    });
                }
                pending.reject(&error);
                return Err(error);
            }
        };
        match (pending.purpose, value) {
            (
                RequestPurpose::Keep,
                ReplyValue::Control(ControlReplyStatus::Prepared { remaining_lease_ms }),
            ) => {
                self.renew(pending.sent_at, remaining_lease_ms);
            }
            (
                RequestPurpose::Recheck,
                ReplyValue::Control(ControlReplyStatus::Prepared { remaining_lease_ms }),
            ) => {
                self.renew(pending.sent_at, remaining_lease_ms);
                if !matches!(self.operation, Some(Operation::Check { .. })) {
                    return Err(unexpected_reply());
                }
                if let Err(error) = self.worker.observe() {
                    self.invalidate_preparation(error.into())?;
                }
            }
            (RequestPurpose::Start, ReplyValue::StartAccepted) => {
                self.set_phase(SessionPhase::Accepted);
                self.state
                    .send_modify(|s| s.submission = crate::StartSubmission::Accepted);
                let Some(operation) = self.operation.take() else {
                    // The caller already received Unknown when the Start
                    // reply deadline elapsed. This late reply updates observers.
                    return Ok(());
                };
                let (reply, delivered) = match operation {
                    Operation::Starting {
                        reply, delivered, ..
                    } => (reply, delivered),
                    operation @ Operation::Delivering { .. } => {
                        // Unknown has been sent, but its caller has not yet
                        // confirmed delivery. Keep that cancellation guard.
                        self.operation = Some(operation);
                        return Ok(());
                    }
                    _ => return Err(unexpected_reply()),
                };
                if reply.send(Ok(StartOutcome::Accepted)).is_err() {
                    self.abandon_start()?;
                } else {
                    self.operation = Some(Operation::Delivering {
                        received: delivered,
                        deadline: Instant::now() + self.timeouts.cleanup_observation,
                    });
                }
            }
            (
                RequestPurpose::Cleanup(reply),
                ReplyValue::Control(ControlReplyStatus::CleanupRequested),
            ) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            (purpose, _) => {
                if let RequestPurpose::Cleanup(Some(reply)) = purpose {
                    let _ = reply.send(Err(unexpected_reply()));
                }
                return Err(unexpected_reply());
            }
        }
        Ok(())
    }

    fn renew(&mut self, sent_at: Instant, remaining_ms: u32) {
        self.lease_deadline = sent_at + std::time::Duration::from_millis(u64::from(remaining_ms));
        self.renew_at = renewal(self.lease_deadline);
    }

    fn exit(&mut self, frame: ReceivedControlFrame) -> Result<(), BrokerSessionError> {
        let exit = self.decode_exit(frame)?;
        self.wire_state
            .exit(self.request.request_id(), exit.is_final())
            .map_err(crate::session::wire::sequence)?;
        let phase = if exit.is_final() {
            SessionPhase::Finished
        } else {
            self.limit_cleanup_observation(Instant::now() + self.timeouts.cleanup_observation);
            SessionPhase::Closing
        };
        self.state.send_modify(|state| {
            state.phase = phase;
            state.report = Some(exit);
        });
        if let Some(operation) = self.operation.take() {
            match operation {
                Operation::Starting { reply, .. } => {
                    let _ = reply.send(Ok(StartOutcome::Reported(exit)));
                }
                operation => {
                    operation.reject(BrokerSessionError::InvalidState(phase));
                }
            }
        }
        if let Some(pending) = self.pending.as_mut() {
            if let RequestPurpose::Cleanup(reply) = &mut pending.purpose {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Ok(()));
                }
            }
        }
        if let Some(cleanup) = self.cleanup.take() {
            if let Some(reply) = cleanup.reply {
                let _ = reply.send(Ok(()));
            }
        }
        if let Some(command) = self.queued.take() {
            command.reject(BrokerSessionError::InvalidState(phase));
        }
        self.worker = Worker::Unavailable;
        Ok(())
    }

    pub(super) fn decode_exit(
        &self,
        frame: ReceivedControlFrame,
    ) -> Result<ExitReport, BrokerSessionError> {
        let exit = self.hello.protocol().decode_exit(
            &self.request,
            self.token.preparation_id(),
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )?;
        Ok(exit.report())
    }
}
