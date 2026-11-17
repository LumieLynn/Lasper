use super::control::RequestPurpose;
use super::operation::Operation;
use super::Owner;
use crate::session::{BrokerSessionError, Command, SessionPhase, StartOutcome};
use std::sync::Arc;
use tokio::time::Instant;

impl Owner<'_> {
    pub(super) fn check_deadlines(&mut self) -> Result<(), BrokerSessionError> {
        let now = Instant::now();
        // A fully sent Start has an uncertain outcome when only its reply is
        // late. Retain correlation and the connection so Close or a later reply
        // can still resolve the session. Partial frame I/O keeps its deadline.
        if self.write_deadline.is_none()
            && matches!(self.operation, Some(Operation::Starting { deadline, .. }) if now >= deadline)
        {
            if let Some(Operation::Starting {
                reply, delivered, ..
            }) = self.operation.take()
            {
                if reply
                    .send(Ok(StartOutcome::Unknown {
                        cause: Arc::new(BrokerSessionError::Deadline),
                    }))
                    .is_err()
                {
                    self.abandon_start()?;
                } else {
                    self.operation = Some(Operation::Delivering {
                        received: delivered,
                        deadline: now + self.timeouts.cleanup_observation,
                    });
                }
            }
        }
        if self.frame_deadline.is_some_and(|deadline| now >= deadline)
            || self.write_deadline.is_some_and(|deadline| now >= deadline)
            || self
                .cleanup_deadline
                .is_some_and(|deadline| now >= deadline)
            || self
                .pending
                .as_ref()
                .filter(|request| self.pending_deadline_applies(&request.purpose))
                .is_some_and(|request| now >= request.deadline)
        {
            return Err(BrokerSessionError::Deadline);
        }
        if self.phase() == SessionPhase::Prepared
            && (now >= self.lease_deadline
                || self
                    .operation
                    .as_ref()
                    .is_some_and(|operation| now >= operation.deadline()))
        {
            self.invalidate_preparation(BrokerSessionError::Deadline)?;
        } else if self
            .operation
            .as_ref()
            .is_some_and(|operation| now >= operation.deadline())
        {
            return Err(BrokerSessionError::Deadline);
        }
        Ok(())
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        [
            self.frame_deadline,
            self.write_deadline,
            self.cleanup_deadline,
            self.pending
                .as_ref()
                .filter(|request| self.pending_deadline_applies(&request.purpose))
                .map(|request| request.deadline),
            self.operation.as_ref().map(Operation::deadline),
            self.queued.as_ref().map(Command::deadline),
            (self.phase() == SessionPhase::Prepared).then_some(self.lease_deadline),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn pending_deadline_applies(&self, purpose: &RequestPurpose) -> bool {
        self.state.borrow().report.is_none() && !matches!(purpose, RequestPurpose::Start)
    }
}
