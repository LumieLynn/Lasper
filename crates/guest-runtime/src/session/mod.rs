mod account;
mod children;
mod execution;
mod guardian;
mod lifecycle;
mod linux;
mod memory;
mod process;
mod ready;
mod status;
mod worker;

pub use children::CleanupProgress;
pub use guardian::{GuardianObservation, SessionGuardian};
pub use lifecycle::{
    CleanupReason, ExecOutcome, SessionAction, SessionConnection, SessionLifecycle,
    SessionLifecycleError, SessionPhase, StartDecision,
};
pub use memory::{WORKER_MEMORY_BYTES, WORKER_STACK_BUDGET_BYTES};
pub use process::WorkerObservation;
pub use ready::{WorkerDescriptors, WorkerReady};
pub use status::{WorkerFailure, WorkerStage};
pub use worker::WorkerBootstrap;

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
    ReadyAlreadyTaken,
    AlreadyStarted,
    InvalidStart(lasper_guest_protocol::SessionPayloadError),
    InvalidStatus,
    Lifecycle(SessionLifecycleError),
}
