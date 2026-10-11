//! TUI application composition root.
//!
//! Core contains the application foundations. Integrations adapt feature-owned
//! commands and completions to App; components retain their own local state.

mod core;
mod integrations;
#[cfg(test)]
mod tests;

pub(crate) use core::run;
pub use core::App;

pub(crate) fn draw(
    frame: &mut ratatui::Frame,
    app: &mut App,
) -> Option<crate::tui::foundation::term::CursorStyle> {
    core::layout::render(frame, app)
}
#[cfg(test)]
pub use core::{AppUi, ModalLayer};
