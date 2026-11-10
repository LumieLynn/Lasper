use core::cell::Cell;
use core::marker::PhantomData;
use lasper_guest_protocol::{BrokerId, EnforcedControlLimits, PreparationId, PrepareRequestFrame};

use crate::session::{GuardianError, SessionGuardian, WorkerBootstrap};
use crate::transport::GuestControlStream;

use super::hello::CompletedHello;
use super::supervisor::{Supervisor, SupervisorStatus};
use crate::SupervisorSignal;

/// Process-local authority to create one worker after broker Go and Hello.
/// The connection, broker liveness and signal FD stay with this guardian.
pub struct GuardianContext {
    connection: GuestControlStream,
    supervisor: Supervisor,
    broker_id: BrokerId,
    hello: CompletedHello,
    session_created: Cell<bool>,
    not_send: PhantomData<*mut ()>,
}

impl GuardianContext {
    pub(super) fn new(
        connection: GuestControlStream,
        supervisor: Supervisor,
        broker_id: BrokerId,
        hello: CompletedHello,
    ) -> Self {
        Self {
            connection,
            supervisor,
            broker_id,
            hello,
            session_created: Cell::new(false),
            not_send: PhantomData,
        }
    }

    pub(in crate::session) const fn hello_request_id(&self) -> u64 {
        self.hello.request_id
    }

    /// Owns this connection through Prepare, execution and descendant cleanup.
    /// Returns only after all assigned children have been reaped.
    pub fn run_session(self) -> Result<(), GuardianError> {
        super::super::control::run(&self)
    }

    pub const fn broker_id(&self) -> BrokerId {
        self.broker_id
    }

    pub const fn connection(&self) -> &GuestControlStream {
        &self.connection
    }

    pub const fn negotiated_limits(&self) -> EnforcedControlLimits {
        self.hello.limits
    }

    pub const fn hello_completed_at_ms(&self) -> u64 {
        self.hello.completed_at_ms
    }

    /// Control readability, broker liveness and pending supervisor signals.
    pub fn poll_descriptors(&self) -> [i32; 3] {
        let [broker, signals] = self.supervisor.descriptors();
        [self.connection.as_raw_fd(), broker, signals]
    }

    pub fn receive_signal(&self) -> Result<Option<SupervisorSignal>, GuardianError> {
        self.supervisor.receive_signal()
    }

    /// A consumed stop or broker EOF remains observable on later loop iterations.
    pub fn supervisor_status(&self) -> Result<SupervisorStatus, GuardianError> {
        self.supervisor.status()
    }

    pub fn spawn<'a>(
        &self,
        request: &'a PrepareRequestFrame<'a>,
        preparation: PreparationId,
        now_ms: u64,
        entry: fn(WorkerBootstrap<'_>) -> !,
    ) -> Result<SessionGuardian<'a, '_>, GuardianError> {
        if self.session_created.get() {
            return Err(GuardianError::SessionAlreadyCreated);
        }
        if request.broker_id() != self.broker_id {
            return Err(GuardianError::MismatchedBroker {
                expected: self.broker_id,
                actual: request.broker_id(),
            });
        }
        self.supervisor.ensure_alive()?;
        self.session_created.set(true);
        // SAFETY: Only the non-returning fork entry can create this context.
        // It owns child creation/reaping and permits only one worker attempt.
        unsafe {
            SessionGuardian::spawn(
                &self.connection,
                self.hello.limits,
                request,
                preparation,
                now_ms,
                entry,
            )
        }
    }
}
