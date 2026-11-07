mod children;
mod guardian;
mod lifecycle;
mod linux;
mod process;
mod status;
mod worker;

pub use children::CleanupProgress;
pub use guardian::{GuardianObservation, SessionGuardian};
pub use lifecycle::{
    CleanupReason, ExecOutcome, PreparedSession, SessionAction, SessionConnection,
    SessionLifecycle, SessionLifecycleError, SessionPhase,
};
pub use process::{WorkerDescriptors, WorkerObservation};
pub use status::{WorkerFailure, WorkerStage};
pub use worker::{StartedWorker, WorkerBootstrap};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerExit {
    Exited(i32),
    Signaled(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianError {
    System { operation: &'static str, errno: i32 },
    ExistingChildren,
    NotPrepared,
    AlreadyStarted,
    InvalidStart(lasper_guest_protocol::SessionPayloadError),
    InvalidStatus,
    Lifecycle(SessionLifecycleError),
}
