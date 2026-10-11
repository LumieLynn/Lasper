mod appearance;
pub mod attrs;
pub mod color;
mod cursor;
mod rio;

pub(crate) use appearance::{
    default_query_colors, probe_host_appearance, query_colors_with_background,
};
pub use color::Color;
pub(crate) use cursor::{CursorStyle, HostCursor, HostCursorStyle};
pub use rio::{
    BrowserMotion, BrowserSelectionKind, Cell, MouseProtocolEncoding, MouseProtocolMode,
    TerminalCore, TerminalKeyProtocol, VtEvent,
};
