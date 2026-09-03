mod contract;
mod service;

#[allow(unused_imports)]
pub use contract::{
    GuestUserNameError, JournalSessionHandle, JournalSessionRequest, ObservedGuestIdentity,
    SessionError, SessionPort, SessionSendStatus, ShellOpenIntent, ShellTarget,
    TerminalSessionHandle, TerminalSessionInput, TerminalSessionRequest, ValidatedGuestUserName,
    WaylandPreparationRequest, WaylandSessionContext, WaylandShellRequest,
};
pub use service::SessionService;

pub(crate) use contract::{
    journal_session_channel, terminal_session_channel, TerminalCommand, TerminalLaunch,
    TerminalSessionEndpoint, TypedSessionEnvironment,
};
#[cfg(test)]
pub(crate) use contract::{JOURNAL_OUTPUT_CAPACITY, TERMINAL_COMMAND_CAPACITY};
