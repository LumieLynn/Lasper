//! I/O with the outer terminal, separate from embedded PTY parsing.
//!
//! Capture appearance before input dispatch starts; emit clipboard and cursor
//! requests through the TUI's terminal output, never back through a guest PTY.

mod appearance;
mod clipboard;

pub(crate) use super::cursor::{CursorStyle, HostCursor, HostCursorStyle};
pub(crate) use appearance::{
    default_query_colors, probe_host_appearance, query_colors_with_defaults,
};
pub(crate) use clipboard::set_clipboard;
pub use clipboard::ClipboardTarget;
