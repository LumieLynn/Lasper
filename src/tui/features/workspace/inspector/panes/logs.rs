use ratatui::{layout::Rect, style::Style, widgets::Paragraph, Frame};

use crate::tui::features::workspace::inspector::presentation::empty_block;
use crate::tui::features::workspace::inspector::DetailRenderContext;
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::display::text::{TextContent, TextEntryIndex, WrappedEntries};

pub(crate) fn render(
    frame: &mut Frame,
    data: &DetailRenderContext<'_>,
    area: Rect,
    scroll: usize,
    index: &mut TextEntryIndex,
) {
    if data.machine.is_none() {
        frame.render_widget(empty_block(), area);
    } else if let Some(buffer) = data.logs.as_ref().filter(|buffer| !buffer.lines.is_empty()) {
        WrappedEntries::new(buffer.lines, buffer.first, index).render(frame, area, scroll);
    } else {
        frame.render_widget(
            Paragraph::new("No log output.")
                .style(Style::default().fg(theme::theme().text_secondary)),
            area,
        );
    }
}
