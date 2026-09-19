mod contract;
mod service;

#[allow(unused_imports)]
pub use contract::{
    GuestCommand, GuestCommandError, GuestUserNameError, InteractiveShellEnvironment,
    InteractiveShellEnvironmentError, JournalSessionHandle, JournalSessionRequest,
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    SessionError, SessionPort, SessionSendStatus, ShellOpenError, ShellOpenIntent, ShellTarget,
    TerminalSessionHandle, TerminalSessionInput, TerminalSessionRequest, ValidatedGuestUserName,
    WaylandPreparationRequest, WaylandSessionContext, WaylandShellRequest, X11FilesystemAccess,
    X11ProjectionContext, X11ProjectionProbeRequest,
};
pub use service::SessionService;

pub(crate) use contract::{
    journal_session_channel, terminal_session_channel, TerminalCommand, TerminalLaunch,
    TerminalSessionEndpoint, TypedSessionEnvironment,
};
#[cfg(test)]
pub(crate) use contract::{JOURNAL_OUTPUT_CAPACITY, TERMINAL_COMMAND_CAPACITY};
