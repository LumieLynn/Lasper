//! UI module containing layout and widget rendering logic.

use ratatui::style::Color;

/// Severity level for status messages shown in the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// Border color for top-level panels (container list, detail, terminal).
pub fn panel_border_color(resize_mode: bool, focused: bool, unfocused: Color) -> Color {
    if resize_mode {
        if focused { Color::Yellow } else { Color::Rgb(180, 140, 0) }
    } else if focused {
        Color::Cyan
    } else {
        unfocused
    }
}

/// Border color for inner widgets (selectable lists, checklists, inputs).
pub fn widget_border_color(focused: bool, enabled: bool) -> Color {
    if !enabled { Color::DarkGray }
    else if focused { Color::Cyan }
    else { Color::White }
}

pub mod core;
pub mod layout;
pub mod views;
pub mod widgets;
pub mod wizard;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use crate::app::App;

/// Draws the entire application UI to the frame.
pub fn draw(f: &mut Frame, app: &mut App) {
    layout::render(f, app);
}

pub fn centered_rect(w_pct: u16, h_pct: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(vert[1])[1]
}
