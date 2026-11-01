//! Guest process requests shared by shells, display observations and commands.
//! Login prompts remain a separate machine operation.

mod bounded;
mod command;
mod environment;
mod port;
mod readiness;
mod request;
mod target;

pub use command::{GuestCommand, GuestCommandError};
pub use environment::{InteractiveShellEnvironment, InteractiveShellEnvironmentError};
pub use target::{
    GuestTarget, GuestUserNameError, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, ValidatedGuestUserName,
};

#[cfg(test)]
pub(crate) use bounded::collect_output;
pub(crate) use bounded::{
    run_bounded, GuestCommandOutput, GuestOutputBoundary, GuestOutputEnd, GuestOutputLimits,
};
pub(crate) use environment::GuestEnvironment;
pub(crate) use port::{
    GuestExecutionError, GuestExecutionFailure, GuestExecutionObservation, GuestExecutionPort,
    PreparedGuestExecution,
};
pub(crate) use readiness::NativeReadiness;
pub(crate) use request::{GuestExecutionRequest, GuestProcess};
