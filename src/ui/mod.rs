//! UI module containing layout and widget rendering logic.

pub mod container_list;
pub mod detail_panel;
pub mod layout;
pub mod widgets;
pub mod wizard;

use ratatui::Frame;

use crate::app::App;

/// Draws the entire application UI to the frame.
pub fn draw(f: &mut Frame, app: &mut App) {
    layout::render(f, app);
}
