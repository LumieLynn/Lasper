use super::control::{unexpected_reply, ReplyValue, RequestPurpose};
use super::operation::Operation;
use super::worker::Worker;
use super::{renewal, Owner};
use crate::session::{BrokerSessionError, SessionExit, SessionPhase, OPERATION_TIMEOUT};
use crate::ReceivedControlFrame;
use lasper_guest_protocol::{ControlReplyStatus, ExitFrame, MessageKind};
use std::sync::Arc;
use tokio::time::Instant;

impl Owner<'_> {
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
        if let Some(expected) = self.retired {
            if expected.header().request_id() == frame.header().request_id() {
                match expected.decode(&self.request, self.token.preparation_id(), &frame) {
                    Ok(_) | Err(BrokerSessionError::Rejected(_)) => {}
                    Err(error) => return Err(error),
                }
                self.retired = None;
                return Ok(());
            }
        }
        let pending = self.pending.take().ok_or_else(unexpected_reply)?;
        let value =
            match pending
                .expected
                .decode(&self.request, self.token.preparation_id(), &frame)
            {
                Ok(value) => value,
                Err(error) => {
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
                let Some(Operation::Starting {
                    reply, delivered, ..
                }) = self.operation.take()
                else {
                    return Err(unexpected_reply());
                };
                self.set_phase(SessionPhase::Started);
                let terminal = self.worker.take_terminal();
                if reply.send(Ok(terminal)).is_err() {
                    self.abandon_start()?;
                } else {
                    self.operation = Some(Operation::Delivering {
                        received: delivered,
                        deadline: Instant::now() + OPERATION_TIMEOUT,
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
        let phase = if exit.report().is_final() {
            SessionPhase::Closed
        } else {
            self.limit_cleanup_observation(Instant::now() + OPERATION_TIMEOUT);
            SessionPhase::Closing
        };
        self.state.send_modify(|state| {
            state.phase = phase;
            state.exit = Some(Arc::clone(&exit));
        });
        if let Some(operation) = self.operation.take() {
            let error = if matches!(&operation, Operation::Starting { .. }) {
                BrokerSessionError::StartInterrupted(Arc::clone(&exit))
            } else {
                BrokerSessionError::InvalidState(phase)
            };
            operation.reject(error);
        }
        if let Some(pending) = self.pending.take() {
            if let RequestPurpose::Cleanup(Some(reply)) = pending.purpose {
                let _ = reply.send(Ok(()));
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
        self.retired = None;
        Ok(())
    }

    pub(super) fn decode_exit(
        &self,
        frame: ReceivedControlFrame,
    ) -> Result<Arc<SessionExit>, BrokerSessionError> {
        let exit = ExitFrame::decode_for(
            &self.request,
            self.token.preparation_id(),
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )?;
        Ok(Arc::new(SessionExit::decode(exit)?))
    }
}
