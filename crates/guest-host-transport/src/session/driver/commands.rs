use super::control::{CleanupRequest, ExpectedReply, Outbound, PendingRequest, RequestPurpose};
use super::operation::{CheckPurpose, Operation, StartPlan};
use super::worker::Worker;
use super::Owner;
use crate::session::{
    wire, BrokerSessionError, Command, SessionPhase, SessionStartOptions, OPERATION_TIMEOUT,
};
use lasper_guest_protocol::{
    PreparationControlFrame, PreparationControlKind, StartRequest, CONTROL_FRAME_HEADER_BYTES,
};
use tokio::time::Instant;

impl Owner<'_> {
    pub(super) fn command(&mut self, command: Command) -> Result<(), BrokerSessionError> {
        if command.is_abandoned() {
            return Ok(());
        }
        if Instant::now() >= command.deadline() {
            command.reject(BrokerSessionError::Deadline);
            return Ok(());
        }
        let allowed = match &command {
            Command::Close { .. } => matches!(
                self.phase(),
                SessionPhase::Prepared | SessionPhase::Starting | SessionPhase::Started
            ),
            _ => self.phase() == SessionPhase::Prepared,
        };
        if !allowed {
            command.reject(BrokerSessionError::InvalidState(self.phase()));
            return Ok(());
        }
        match command {
            Command::Cancel { deadline, reply } => self.begin_cleanup(CleanupRequest {
                kind: PreparationControlKind::Cancel,
                deadline,
                reply: Some(reply),
            }),
            Command::Close { deadline, reply } => self.begin_cleanup(CleanupRequest {
                kind: PreparationControlKind::Close,
                deadline,
                reply: Some(reply),
            }),
            command if self.operation.is_some() || !self.control_available() => {
                self.queued = Some(command);
                Ok(())
            }
            Command::Recheck { deadline, reply } => {
                let (frame, expected) = self.control_frame(PreparationControlKind::Recheck)?;
                self.operation = Some(Operation::Check {
                    purpose: CheckPurpose::Recheck(reply),
                    deadline,
                });
                self.exchange(
                    frame,
                    expected,
                    RequestPurpose::Recheck,
                    deadline.min(self.lease_deadline),
                )
            }
            Command::Start {
                deadline,
                options,
                evidence,
                reply,
                delivered,
            } => {
                let control = self.control_frame(PreparationControlKind::Recheck)?;
                let payload = match self.start_payload(options) {
                    Ok(start) => start,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return Ok(());
                    }
                };
                self.operation = Some(Operation::Check {
                    purpose: CheckPurpose::Start(StartPlan {
                        payload,
                        evidence,
                        reply,
                        delivered,
                    }),
                    deadline,
                });
                self.exchange(
                    control.0,
                    control.1,
                    RequestPurpose::Recheck,
                    deadline.min(self.lease_deadline),
                )
            }
        }
    }

    pub(super) fn begin_cleanup(
        &mut self,
        cleanup: CleanupRequest,
    ) -> Result<(), BrokerSessionError> {
        if let Some(operation) = self.operation.take() {
            operation.reject(BrokerSessionError::InvalidState(SessionPhase::Closing));
        }
        if let Some(pending) = self.pending.take() {
            self.retired = Some(pending.expected);
            pending.reject(&BrokerSessionError::InvalidState(SessionPhase::Closing));
        }
        self.worker = Worker::Unavailable;
        if let Some(command) = self.queued.take() {
            command.reject(BrokerSessionError::InvalidState(SessionPhase::Closing));
        }
        self.limit_cleanup_observation(cleanup.deadline);
        self.set_phase(SessionPhase::Closing);
        self.cleanup = Some(cleanup);
        self.send_cleanup()
    }

    pub(super) fn send_cleanup(&mut self) -> Result<(), BrokerSessionError> {
        if self.connection.control().has_output() || !self.connection.control().can_continue() {
            return Ok(());
        }
        if let Some(cleanup) = self.cleanup.take() {
            let (frame, expected) = self.control_frame(cleanup.kind)?;
            self.exchange(
                frame,
                expected,
                RequestPurpose::Cleanup(cleanup.reply),
                cleanup.deadline,
            )?;
        }
        Ok(())
    }

    pub(super) fn keep_prepared(&mut self) -> Result<(), BrokerSessionError> {
        let deadline = self.lease_deadline.min(Instant::now() + OPERATION_TIMEOUT);
        let (frame, expected) = self.control_frame(PreparationControlKind::KeepPrepared)?;
        self.exchange(frame, expected, RequestPurpose::Keep, deadline)
    }

    pub(super) fn exchange(
        &mut self,
        frame: Outbound,
        expected: ExpectedReply,
        purpose: RequestPurpose,
        deadline: Instant,
    ) -> Result<(), BrokerSessionError> {
        let sent_at = Instant::now();
        let queued = (|| {
            if Instant::now() >= deadline {
                return Err(BrokerSessionError::Deadline);
            }
            // The guardian can outlive the broker or machine leader. Let
            // cleanup reach it on this connection even if recheck would fail.
            if !matches!(
                expected,
                ExpectedReply::Control { command, .. }
                    if matches!(command.kind(), PreparationControlKind::Cancel | PreparationControlKind::Close)
            ) {
                self.connection.recheck()?;
            }
            self.connection
                .control()
                .queue_frame(frame.header, frame.payload)?;
            Ok(())
        })();
        if let Err(error) = queued {
            PendingRequest {
                expected,
                sent_at,
                deadline,
                purpose,
            }
            .reject(&error);
            return Err(error);
        }
        self.output_deadline = Some(deadline);
        self.pending = Some(PendingRequest {
            expected,
            sent_at,
            deadline,
            purpose,
        });
        Ok(())
    }

    pub(super) fn control_frame(
        &mut self,
        kind: PreparationControlKind,
    ) -> Result<(Outbound, ExpectedReply), BrokerSessionError> {
        let command = PreparationControlFrame::new(self.request_id()?, kind, self.token);
        let mut payload = [0; 512];
        let (header, length) = command.encode_parts(&mut payload)?;
        Ok((
            Outbound {
                header,
                payload: payload[..length].into(),
            },
            ExpectedReply::Control { header, command },
        ))
    }

    fn start_payload(&self, options: SessionStartOptions) -> Result<Box<[u8]>, BrokerSessionError> {
        let start = StartRequest::new(
            &self.request,
            self.token.preparation_id(),
            options.wayland_display.as_deref(),
            options.x11_display,
            options.terminal_size,
        )
        .map_err(wire::encoding)?;
        let maximum = self
            .connection
            .readiness()
            .limits()
            .get()
            .max_control_frame_bytes() as usize
            - CONTROL_FRAME_HEADER_BYTES;
        wire::encode(maximum, |output| start.encode_payload(output))
    }
}
