//! Asynchronous presentation effects that translate host results into TUI events.

mod backend;
pub(crate) mod metrics;

pub(crate) use backend::{handle_command, BackendCommand, BackendResponse};
