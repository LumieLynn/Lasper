mod contract;
pub(crate) mod execution;
mod service;
mod shell;

#[allow(unused_imports)]
pub use contract::{
    JournalSessionHandle, JournalSessionRequest, SessionError, SessionPort, SessionSendStatus,
    ShellOpenError, ShellOpenIntent, TerminalOpenError, TerminalSessionHandle,
    TerminalSessionInput, TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext,
    WaylandShellRequest, X11FilesystemAccess, X11ProjectionContext, X11ProjectionProbeRequest,
    X11SessionContext,
};
#[allow(unused_imports)]
pub use execution::{
    GuestCommand, GuestCommandError, GuestTarget, GuestUserNameError, InteractiveShellEnvironment,
    InteractiveShellEnvironmentError, MappedGuestIdentity, ObservedGuestIdentity,
    ObservedMachineInstance, ObservedNamespaceIdentity, ValidatedGuestUserName,
};
pub use service::SessionService;
#[allow(unused_imports)]
pub use shell::{
    ShellAttemptError, ShellLaunchError, ShellLaunchRequest, ShellLaunchResult,
    WaylandFallbackCause,
};

pub(crate) use contract::{
    journal_session_channel, terminal_session_channel, TerminalCommand, TerminalLaunch,
    TerminalSessionEndpoint, TypedSessionEnvironment,
};
#[cfg(test)]
pub(crate) use contract::{JOURNAL_OUTPUT_CAPACITY, TERMINAL_COMMAND_CAPACITY};
