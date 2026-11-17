use super::GuestOperation;

/// Progress of the wire exchange after Hello, shared by both peers. Names such
/// as ReadyReceived describe the host's view, not the worker's execution state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireSessionPhase {
    Established,
    PrepareSent,
    PrepareRejected,
    ReadyReceived,
    PreparedReplyPending,
    StartSent,
    StartRejected,
    StartAcceptedReceived,
    Closing,
    ExitReceived,
    ConnectionClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireReply {
    Ready,
    Prepared,
    StartAccepted,
    CleanupRequested,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireSessionError {
    InvalidRequestId,
    InvalidState { phase: WireSessionPhase },
    UnexpectedReply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Request {
    id: u64,
    operation: GuestOperation,
}

/// Enforces the same message order at both ends of a connection. Call response
/// methods only after the selected codec has checked fields, IDs and FD counts.
/// For outgoing responses, validate a copy and commit it after the full write.
/// For outgoing requests, reserve with `queue_request` and commit with
/// `request_sent`, so replies can arrive while the write is pending. An abandoned
/// partial write ends the connection without confirming the operation's outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireSessionState {
    phase: WireSessionPhase,
    last_request_id: u64,
    prepare_id: Option<u64>,
    pending: Option<Request>,
    cleanup: Option<Request>,
    // A rejected cleanup must restore the phase, including any intervening reply.
    cleanup_origin: Option<WireSessionPhase>,
    queued: Option<Request>,
    final_exit: bool,
}

impl WireSessionState {
    /// Starts after successful Hello selection. The Hello request ID reserves
    /// the beginning of this connection's strictly increasing request sequence.
    pub const fn after_hello(hello_request_id: u64) -> Self {
        Self {
            phase: WireSessionPhase::Established,
            last_request_id: hello_request_id,
            prepare_id: None,
            pending: None,
            cleanup: None,
            cleanup_origin: None,
            queued: None,
            final_exit: false,
        }
    }

    pub const fn phase(&self) -> WireSessionPhase {
        self.phase
    }

    /// Reserves the host's next request before asynchronous writing starts.
    /// The reservation preserves response correlation if Exit arrives before
    /// the write completes.
    pub fn queue_request(
        &mut self,
        id: u64,
        operation: GuestOperation,
    ) -> Result<(), WireSessionError> {
        if self.queued.is_some() {
            return Err(WireSessionError::InvalidState { phase: self.phase });
        }
        let mut checked = *self;
        checked.request(id, operation)?;
        self.queued = Some(Request { id, operation });
        Ok(())
    }

    /// Commits the reserved request after its last byte has been written.
    pub fn request_sent(&mut self) -> Result<(), WireSessionError> {
        let request = self
            .queued
            .take()
            .ok_or(WireSessionError::InvalidState { phase: self.phase })?;
        if self.phase == WireSessionPhase::ExitReceived {
            self.last_request_id = request.id;
            if matches!(
                request.operation,
                GuestOperation::Cancel | GuestOperation::Close
            ) {
                self.cleanup = Some(request);
            } else {
                self.pending = Some(request);
            }
            Ok(())
        } else {
            self.request(request.id, request.operation)
        }
    }

    /// Records a complete host request. A fresh ID is consumed even when the
    /// operation is out of sequence; the guest rejects that request without
    /// changing the phase or registering a response expectation. Payload
    /// validation is separate; a rejected payload completes with `WireReply::Rejected`.
    pub fn request(&mut self, id: u64, operation: GuestOperation) -> Result<(), WireSessionError> {
        if self.queued.is_some() {
            return Err(WireSessionError::InvalidState { phase: self.phase });
        }
        if id <= self.last_request_id {
            return Err(WireSessionError::InvalidRequestId);
        }
        self.last_request_id = id;
        use GuestOperation as Op;
        use WireSessionPhase as Phase;
        let phase = match (self.phase, operation) {
            (Phase::Established, Op::Prepare) => Phase::PrepareSent,
            (Phase::ReadyReceived, Op::KeepPrepared | Op::Recheck) => Phase::PreparedReplyPending,
            (Phase::ReadyReceived, Op::Start) => Phase::StartSent,
            (Phase::ReadyReceived | Phase::PreparedReplyPending, Op::Cancel | Op::Close)
            | (Phase::StartSent | Phase::StartRejected | Phase::StartAcceptedReceived, Op::Close) => {
                Phase::Closing
            }
            _ => return Err(WireSessionError::InvalidState { phase: self.phase }),
        };
        let request = Request { id, operation };
        if matches!(operation, Op::Cancel | Op::Close) {
            self.cleanup = Some(request);
            self.cleanup_origin = Some(self.phase);
        } else {
            self.pending = Some(request);
        }
        if operation == Op::Prepare {
            self.prepare_id = Some(id);
        }
        self.phase = phase;
        Ok(())
    }

    /// Completes the matching request. Replies already in flight may follow Exit
    /// or overlap Close; they do not reopen a completed or closing session.
    pub fn reply(
        &mut self,
        id: u64,
        operation: GuestOperation,
        reply: WireReply,
    ) -> Result<(), WireSessionError> {
        use GuestOperation as Op;
        use WireReply as Reply;
        use WireSessionPhase as Phase;
        if self.phase == Phase::ConnectionClosed {
            return Err(WireSessionError::UnexpectedReply);
        }
        let request = Request { id, operation };
        let cleanup = matches!(operation, Op::Cancel | Op::Close);
        let expected = if cleanup { self.cleanup } else { self.pending };
        if expected != Some(request) {
            return Err(WireSessionError::UnexpectedReply);
        }
        let next = match (operation, reply) {
            (Op::Prepare, Reply::Ready) => Phase::ReadyReceived,
            (Op::Prepare, Reply::Rejected) => Phase::PrepareRejected,
            (Op::KeepPrepared | Op::Recheck, Reply::Prepared | Reply::Rejected) => {
                Phase::ReadyReceived
            }
            (Op::Start, Reply::StartAccepted) => Phase::StartAcceptedReceived,
            (Op::Start, Reply::Rejected) => Phase::StartRejected,
            (Op::Cancel | Op::Close, Reply::CleanupRequested | Reply::Rejected) => Phase::Closing,
            _ => return Err(WireSessionError::UnexpectedReply),
        };
        let phase = match self.phase {
            Phase::ExitReceived => Phase::ExitReceived,
            Phase::Closing if cleanup && reply == Reply::Rejected => self
                .cleanup_origin
                .ok_or(WireSessionError::UnexpectedReply)?,
            Phase::Closing => Phase::Closing,
            _ => next,
        };
        if cleanup {
            self.cleanup = None;
            self.cleanup_origin = None;
        } else {
            self.pending = None;
            // A response can overtake cleanup. If cleanup is later rejected,
            // restore the phase reached by this response, not the older phase.
            if self.phase == Phase::Closing && self.cleanup_origin.is_some() {
                self.cleanup_origin = Some(next);
            }
        }
        self.phase = phase;
        Ok(())
    }

    /// Records a report for the original Prepare. Non-final reports may be
    /// followed by cleanup updates; a final report permits no further Exit.
    pub fn exit(
        &mut self,
        prepare_request_id: u64,
        final_report: bool,
    ) -> Result<(), WireSessionError> {
        use WireSessionPhase as Phase;
        if self.prepare_id != Some(prepare_request_id)
            || self.final_exit
            || !matches!(
                self.phase,
                Phase::ReadyReceived
                    | Phase::PreparedReplyPending
                    | Phase::StartSent
                    | Phase::StartRejected
                    | Phase::StartAcceptedReceived
                    | Phase::Closing
                    | Phase::ExitReceived
            )
        {
            return Err(WireSessionError::UnexpectedReply);
        }
        self.phase = Phase::ExitReceived;
        self.final_exit = final_report;
        Ok(())
    }

    /// Closing the wire does not assert that worker cleanup has completed.
    pub fn disconnect(&mut self) {
        self.phase = WireSessionPhase::ConnectionClosed;
        self.pending = None;
        self.cleanup = None;
        self.cleanup_origin = None;
        self.queued = None;
    }
}
