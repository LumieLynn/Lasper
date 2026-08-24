mod contract;
mod service;

pub use contract::{
    JournalSessionHandle, JournalSessionRequest, SessionError, SessionPort, SessionSendStatus,
    TerminalSessionHandle, TerminalSessionInput, TerminalSessionRequest,
};
pub use service::SessionService;

pub(crate) use contract::{
    journal_session_channel, terminal_session_channel, TerminalCommand, TerminalSessionEndpoint,
};

#[cfg(test)]
pub(crate) use contract::{JOURNAL_OUTPUT_CAPACITY, TERMINAL_COMMAND_CAPACITY};
