//! Ends host observation while preserving any complete reports already queued.
//!
//! The final read has a fixed byte budget and never waits for a missing tail.
//! A retained report does not clear the original host error. After disconnect,
//! guest cleanup continues without a channel for confirming its result here.

use super::control::{unexpected_reply, ExpectedReply, ReplyValue};
use super::worker::Worker;
use super::Owner;
use crate::session::{BrokerSessionError, SessionPhase, StartSubmission};
use crate::ReceivedControlFrame;
use lasper_guest_protocol::{ExitReport, MessageKind};
use std::sync::Arc;

impl Owner<'_> {
    pub(super) fn fail(&mut self, error: BrokerSessionError, first: Option<ReceivedControlFrame>) {
        let (exit, submission) = self.available_exit(first);
        self.wire_state.disconnect();
        let error = match self.operation.take() {
            Some(operation) => operation.reject(error),
            None => error,
        };
        if let Some(pending) = self.pending.take() {
            pending.reject(&error);
        }
        if let Some(command) = self.queued.take() {
            command.reject(error.clone());
        }
        if let Some(cleanup) = self.cleanup.take() {
            if let Some(reply) = cleanup.reply {
                let _ = reply.send(Err(error.clone()));
            }
        }
        self.worker = Worker::Unavailable;
        self.state.send_modify(|state| {
            state.phase = SessionPhase::Finished;
            state.submission = submission;
            state.error.get_or_insert_with(|| Arc::new(error));
            if let Some(exit) = exit {
                state.report = Some(exit);
            }
        });
    }

    fn available_exit(
        &mut self,
        mut first: Option<ReceivedControlFrame>,
    ) -> (Option<ExitReport>, StartSubmission) {
        let mut submission = self.state.borrow().submission;
        if self
            .state
            .borrow()
            .report
            .is_some_and(|report| report.is_final())
        {
            return (None, submission);
        }
        // Replies and progress reports can precede the final Exit. Consume
        // only the bytes queued now, retaining submission facts without resuming
        // operations or waiting for more input. A poisoned reader cannot safely
        // resume at a frame boundary.
        let mut budget = self.connection.control().queued_input_bytes().unwrap_or(0);
        let mut expected = self.pending.as_ref().map(|request| request.expected);
        let mut retired = self.retired;
        let mut exit = None;
        loop {
            let received = match first.take() {
                Some(frame) => Ok(Some(frame)),
                None => self.connection.control().try_receive_available(&mut budget),
            };
            let frame = match received {
                Ok(Some(frame)) => frame,
                Ok(None) | Err(_) => break,
            };
            match self.inspect_failed_frame(
                frame,
                &mut expected,
                &mut retired,
                &mut exit,
                &mut submission,
            ) {
                Ok(true) | Err(_) => break,
                Ok(false) => {}
            }
        }
        (exit, submission)
    }

    fn inspect_failed_frame(
        &mut self,
        frame: ReceivedControlFrame,
        expected: &mut Option<ExpectedReply>,
        retired: &mut Option<ExpectedReply>,
        exit: &mut Option<ExitReport>,
        submission: &mut StartSubmission,
    ) -> Result<bool, BrokerSessionError> {
        if frame.header().kind() == MessageKind::Exit {
            let report = self.decode_exit(frame)?;
            let final_report = report.is_final();
            self.wire_state
                .exit(self.request.request_id(), final_report)
                .map_err(crate::session::wire::sequence)?;
            *exit = Some(report);
            return Ok(final_report);
        }
        let reply = if retired
            .is_some_and(|reply| reply.header().request_id() == frame.header().request_id())
        {
            retired.take()
        } else {
            expected.take()
        }
        .ok_or_else(unexpected_reply)?;
        match self.decode_reply(reply, &frame) {
            Ok(ReplyValue::StartAccepted) => {
                *submission = StartSubmission::Accepted;
                Ok(false)
            }
            Err(BrokerSessionError::Rejected(_)) if matches!(reply, ExpectedReply::Start(_)) => {
                *submission = StartSubmission::Rejected;
                Ok(false)
            }
            Ok(_) | Err(BrokerSessionError::Rejected(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}
