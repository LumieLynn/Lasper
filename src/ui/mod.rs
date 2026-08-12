//! UI module containing layout and widget rendering logic.

use ratatui::style::Color;
use unicode_width::UnicodeWidthChar;

/// Severity level for status messages shown in the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// Border color for top-level panels (container list, detail, terminal).
pub fn panel_border_color(resize_mode: bool, focused: bool, is_primary: bool) -> Color {
    theme::theme().panel_border(resize_mode, focused, is_primary)
}

/// Border color for inner widgets (selectable lists, checklists, inputs).
pub fn widget_border_color(focused: bool, enabled: bool) -> Color {
    theme::theme().widget_border(focused, enabled)
}

/// Wrap text into the terminal rows that will actually be rendered. Lines
/// prefer whitespace boundaries, while long image names and object paths are
/// split by terminal-cell width.
pub(crate) fn soft_wrap_text(value: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut result = Vec::new();

    for source_line in value.split('\n') {
        let mut remaining = source_line;
        if remaining.is_empty() {
            result.push(String::new());
            continue;
        }

        while !remaining.is_empty() {
            let mut current_width = 0;
            let mut hard_end = 0;
            let mut whitespace_end = None;

            for (index, character) in remaining.char_indices() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if hard_end > 0 && current_width + character_width > max_width {
                    break;
                }
                current_width += character_width;
                hard_end = index + character.len_utf8();
                if character.is_whitespace() {
                    whitespace_end = Some(hard_end);
                }
            }

            if hard_end == remaining.len() {
                result.push(remaining.trim_end().to_string());
                break;
            }

            let split_at = whitespace_end.filter(|end| *end > 0).unwrap_or(hard_end);
            let wrapped = remaining[..split_at].trim_end();
            if !wrapped.is_empty() {
                result.push(wrapped.to_string());
            }
            remaining = remaining[split_at..].trim_start();
        }
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

pub mod core;
pub mod layout;
pub mod theme;
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn soft_wrap_prefers_words_and_splits_long_tokens() {
        let wrapped = soft_wrap_text("remove this .oci-sha256:averylongdigest", 12);

        assert_eq!(wrapped[0], "remove this");
        assert!(wrapped
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 12));
        assert!(wrapped.len() > 2);
    }

    #[test]
    fn soft_wrap_uses_terminal_cell_width() {
        let wrapped = soft_wrap_text("路径/very-long-value", 8);

        assert!(wrapped
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 8));
        assert_eq!(wrapped.concat(), "路径/very-long-value");
    }
}
