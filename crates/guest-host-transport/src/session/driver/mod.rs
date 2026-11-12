mod commands;
mod control;
mod deadlines;
mod failure;
mod operation;
mod replies;
mod worker;

use self::control::{CleanupRequest, ExpectedReply, PendingRequest};
use self::operation::Operation;
use self::worker::Worker;
use super::{
    wire, BrokerSessionError, Command, Reply, SessionPhase, SessionSnapshot, KEEP_INTERVAL,
    OPERATION_TIMEOUT,
};
use crate::stream::ControlIoEvent;
use crate::{BrokerConnection, PreparedWorker, WorkerObservation};
use lasper_guest_protocol::{
    ControlFrameHeader, MessageKind, PreparationControl, PreparationControlKind,
    PrepareRequestFrame,
};
use std::future::pending;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep_until, Instant};

pub(super) async fn run(
    mut connection: BrokerConnection,
    header: ControlFrameHeader,
    payload: Box<[u8]>,
    mut commands: mpsc::Receiver<Command>,
    state: watch::Sender<SessionSnapshot>,
    ready: Reply<(lasper_guest_protocol::PreparationId, WorkerObservation)>,
    deadline: Instant,
) {
    let request = match PrepareRequestFrame::decode_parts(
        header,
        &payload,
        0,
        connection.readiness().broker_id(),
    ) {
        Ok(request) => Arc::new(request),
        Err(error) => {
            let _ = ready.send(Err(wire::encoding(error)));
            return;
        }
    };
    let prepared = tokio::select! {
        prepared = prepare(&mut connection, Arc::clone(&request), header, &payload, deadline) => prepared,
        _ = commands.recv() => return,
    };
    let (worker, lease_deadline) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let preparation = worker.ready().response().preparation_id();
    let mut owner = Owner {
        connection,
        request,
        token: PreparationControl::new(worker.ready().response().broker_id(), preparation),
        worker: Worker::Prepared(worker),
        state,
        next_id: header.request_id(),
        lease_deadline,
        renew_at: renewal(lease_deadline),
        pending: None,
        operation: Some(Operation::Prepare {
            reply: ready,
            deadline,
        }),
        queued: None,
        retired: None,
        cleanup: None,
        frame_deadline: None,
        output_deadline: None,
        cleanup_deadline: None,
        detached: false,
    };
    if let Err(error) = owner.worker.observe() {
        owner.fail(error.into(), None);
        return;
    }
    if let Err(error) = owner.drive(&mut commands).await {
        owner.fail(error, None);
    }
}

async fn prepare<'payload>(
    connection: &mut BrokerConnection,
    request: Arc<PrepareRequestFrame<'payload>>,
    header: ControlFrameHeader,
    payload: &[u8],
    deadline: Instant,
) -> Result<(PreparedWorker<'payload>, Instant), BrokerSessionError> {
    // The guest's remaining lease may age in transit. Counting from the send
    // keeps the host's estimate conservative when Ready is delayed.
    let sent_at = Instant::now();
    wire::send(connection, header, payload, deadline).await?;
    let frame = wire::receive(connection, deadline).await?;
    if frame.header().kind() == MessageKind::Error {
        wire::rejection(header, &request, None, &frame)?;
    }
    let worker = PreparedWorker::from_frame(
        Arc::clone(connection.target()),
        connection.readiness(),
        request,
        frame,
    )?;
    let lease_deadline = sent_at
        + std::time::Duration::from_millis(u64::from(
            worker.ready().response().remaining_lease_ms(),
        ));
    Ok((worker, lease_deadline))
}

struct Owner<'payload> {
    connection: BrokerConnection,
    request: Arc<PrepareRequestFrame<'payload>>,
    token: PreparationControl,
    worker: Worker<'payload>,
    state: watch::Sender<SessionSnapshot>,
    next_id: u64,
    lease_deadline: Instant,
    renew_at: Instant,
    pending: Option<PendingRequest>,
    operation: Option<Operation>,
    queued: Option<Command>,
    // Cleanup replaces the outstanding request, whose reply can still arrive.
    // Validate that reply without applying its state changes.
    retired: Option<ExpectedReply>,
    cleanup: Option<CleanupRequest>,
    frame_deadline: Option<Instant>,
    output_deadline: Option<Instant>,
    cleanup_deadline: Option<Instant>,
    detached: bool,
}

impl Owner<'_> {
    fn phase(&self) -> SessionPhase {
        self.state.borrow().phase
    }

    fn set_phase(&self, phase: SessionPhase) {
        self.state.send_modify(|state| state.phase = phase);
    }

    fn limit_cleanup_observation(&mut self, deadline: Instant) {
        self.cleanup_deadline = Some(
            self.cleanup_deadline
                .map_or(deadline, |current| current.min(deadline)),
        );
    }

    fn request_id(&mut self) -> Result<u64, BrokerSessionError> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| BrokerSessionError::InvalidRequest("request ID exhausted".into()))?;
        Ok(self.next_id)
    }

    fn control_available(&mut self) -> bool {
        self.pending.is_none() && !self.connection.control().has_output()
    }

    async fn drive(
        &mut self,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<(), BrokerSessionError> {
        loop {
            if matches!(self.phase(), SessionPhase::Closed | SessionPhase::Failed) {
                return Ok(());
            }
            if commands.is_closed() && !self.detached {
                self.detached = true;
                if self.phase() != SessionPhase::Closing {
                    self.begin_cleanup(CleanupRequest {
                        kind: PreparationControlKind::Close,
                        deadline: Instant::now() + OPERATION_TIMEOUT,
                        reply: None,
                    })?;
                }
            }
            self.expire_queued();
            self.check_deadlines()?;
            self.send_cleanup()?;
            self.submit_start()?;
            if self.operation.is_none() && self.control_available() {
                if let Some(command) = self.queued.take() {
                    self.command(command)?;
                }
            }
            let renew = self.phase() == SessionPhase::Prepared && self.control_available();
            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                _ = wait_until(deadline) => {}
                result = self.worker.advance() => self.observed(result)?,
                event = async {
                    match &mut self.operation {
                        Some(operation) => operation.advance().await,
                        None => pending().await,
                    }
                } => self.operation_event(event)?,
                _ = async {
                    match &mut self.queued {
                        Some(command) => command.closed().await,
                        None => pending().await,
                    }
                } => { self.queued.take(); }
                command = commands.recv(), if self.queued.is_none() && !self.detached => {
                    if let Some(command) = command { self.command(command)?; }
                }
                event = self.connection.control().advance_io() => {
                    match event? {
                        ControlIoEvent::Input(Some(frame)) => self.receive(frame)?,
                        ControlIoEvent::Input(None) => {
                            self.frame_deadline.get_or_insert_with(|| Instant::now() + OPERATION_TIMEOUT);
                        }
                        ControlIoEvent::Sent => { self.output_deadline = None; }
                        ControlIoEvent::WriteFailed(error) => {
                            if self.phase() != SessionPhase::Closing {
                                return Err(error.into());
                            }
                            self.output_deadline = None;
                            self.state.send_modify(|state| {
                                state.error.get_or_insert_with(|| Arc::new(error.into()));
                            });
                        }
                    }
                }
                _ = sleep_until(self.renew_at), if renew => self.keep_prepared()?,
            }
        }
    }

    fn expire_queued(&mut self) {
        if self
            .queued
            .as_ref()
            .is_some_and(|command| Instant::now() >= command.deadline())
        {
            self.queued
                .take()
                .unwrap()
                .reject(BrokerSessionError::Deadline);
        }
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn renewal(deadline: Instant) -> Instant {
    Instant::now() + KEEP_INTERVAL.min(deadline.saturating_duration_since(Instant::now()) / 2)
}
