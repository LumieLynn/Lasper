//! Guest process requests shared by shells, display observations and commands.
//! Login prompts remain a separate machine operation.

mod command;
mod environment;
mod readiness;
mod request;
mod target;

pub use command::{GuestCommand, GuestCommandError};
pub use environment::{InteractiveShellEnvironment, InteractiveShellEnvironmentError};
pub use target::{
    GuestTarget, GuestUserNameError, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, ValidatedGuestUserName,
};

pub(crate) use environment::GuestEnvironment;
pub(crate) use readiness::NativeReadiness;
pub(crate) use request::{GuestExecutionRequest, GuestProcess};
