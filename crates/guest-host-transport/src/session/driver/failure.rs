use super::control::{unexpected_reply, ExpectedReply};
use super::worker::Worker;
use super::Owner;
use crate::session::{BrokerSessionError, SessionExit, SessionPhase};
use crate::ReceivedControlFrame;
use lasper_guest_protocol::MessageKind;
use std::sync::Arc;

impl Owner<'_> {
    pub(super) fn fail(&mut self, error: BrokerSessionError, first: Option<ReceivedControlFrame>) {
        let exit = self.available_exit(first);
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
            state.phase = SessionPhase::Failed;
            state.error.get_or_insert_with(|| Arc::new(error));
            if let Some(exit) = exit {
                state.exit = Some(exit);
            }
        });
    }

    fn available_exit(
        &mut self,
        mut first: Option<ReceivedControlFrame>,
    ) -> Option<Arc<SessionExit>> {
        if self
            .state
            .borrow()
            .exit
            .as_ref()
            .is_some_and(|exit| exit.report().is_final())
        {
            return None;
        }
        // Replies and progress reports can precede the final Exit. Consume
        // only the bytes queued now, without applying replies or waiting for
        // more input. A poisoned reader cannot safely resume at a frame boundary.
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
            match self.inspect_failed_frame(frame, &mut expected, &mut retired, &mut exit) {
                Ok(true) | Err(_) => break,
                Ok(false) => {}
            }
        }
        exit
    }

    fn inspect_failed_frame(
        &self,
        frame: ReceivedControlFrame,
        expected: &mut Option<ExpectedReply>,
        retired: &mut Option<ExpectedReply>,
        exit: &mut Option<Arc<SessionExit>>,
    ) -> Result<bool, BrokerSessionError> {
        if frame.header().kind() == MessageKind::Exit {
            let report = self.decode_exit(frame)?;
            let final_report = report.report().is_final();
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
        match reply.decode(&self.request, self.token.preparation_id(), &frame) {
            Ok(_) | Err(BrokerSessionError::Rejected(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}
