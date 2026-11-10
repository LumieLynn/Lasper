mod account;
mod bootstrap;
mod children;
mod control;
mod error;
mod execution;
mod guardian;
mod lifecycle;
mod memory;
mod prepare;
mod process;
mod projection;
mod ready;
mod status;
mod worker;

pub use bootstrap::{GuardianBootstrap, GuardianContext, SupervisorStatus, GUARDIAN_START_BYTE};
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
pub use status::{WorkerFailure, WorkerStage};
pub use worker::WorkerBootstrap;

pub use crate::linux::ProcessExit as WorkerExit;
