use super::operation::Operation;
use super::Owner;
use crate::session::{BrokerSessionError, Command, SessionPhase};
use tokio::time::Instant;

impl Owner<'_> {
    pub(super) fn check_deadlines(&mut self) -> Result<(), BrokerSessionError> {
        let now = Instant::now();
        if self.frame_deadline.is_some_and(|deadline| now >= deadline)
            || self.output_deadline.is_some_and(|deadline| now >= deadline)
            || self
                .cleanup_deadline
                .is_some_and(|deadline| now >= deadline)
            || self
                .pending
                .as_ref()
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
            self.output_deadline,
            self.cleanup_deadline,
            self.pending.as_ref().map(|request| request.deadline),
            self.operation.as_ref().map(Operation::deadline),
            self.queued.as_ref().map(Command::deadline),
            (self.phase() == SessionPhase::Prepared).then_some(self.lease_deadline),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}
