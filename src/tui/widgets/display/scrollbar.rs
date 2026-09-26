use ratatui::widgets::{Scrollbar, ScrollbarOrientation};

/// Shared vertical scrollbar styling for scrollable TUI panels.
pub(crate) fn vertical_scrollbar() -> Scrollbar<'static> {
    Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("▐")
        .track_symbol(Some("│"))
}
