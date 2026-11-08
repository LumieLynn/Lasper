mod account;
mod bootstrap;
mod children;
mod error;
mod execution;
mod guardian;
mod lifecycle;
mod linux;
mod memory;
mod prepare;
mod process;
mod ready;
mod status;
mod worker;

pub use bootstrap::{
    GuardianBootstrap, GuardianContext, GuardianSignal, SupervisorStatus, GUARDIAN_START_BYTE,
};
pub use children::CleanupProgress;
pub use error::GuardianError;
pub use guardian::{GuardianObservation, SessionGuardian};
pub use lifecycle::{
    CleanupReason, ExecOutcome, SessionAction, SessionConnection, SessionLifecycle,
    SessionLifecycleError, SessionPhase, StartDecision,
};
pub use memory::{WORKER_MEMORY_BYTES, WORKER_STACK_BUDGET_BYTES};
pub use prepare::{PrepareBuffer, PrepareStorage, PrepareStorageError};
pub use process::WorkerObservation;
pub use ready::{WorkerDescriptors, WorkerReady};
pub use status::{WorkerFailure, WorkerStage};
pub use worker::WorkerBootstrap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerExit {
    Exited(i32),
    Signaled(i32),
}
