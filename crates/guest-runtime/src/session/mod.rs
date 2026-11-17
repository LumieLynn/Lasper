//! Runs each connection in a dedicated guardian process with one assigned worker.
//!
//! `control` enforces wire ordering and drives I/O. `SessionGuardian` applies
//! `SessionLifecycle` decisions to the worker resources in `process`. Execution
//! phases remain independent of whether a reply has reached the host.
//! `worker` runs in the forked child and prepares the account, argv and environment
//! before waiting for Start. The guardian retains ownership through child cleanup,
//! including after the control connection has been lost.

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
mod reply;
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
pub use prepare::{DecodedPrepare, PrepareBuffer, PrepareStorage, PrepareStorageError};
pub use process::WorkerObservation;
pub use status::{WorkerFailure, WorkerStage};
pub use worker::WorkerBootstrap;

pub use crate::linux::ProcessExit as WorkerExit;
