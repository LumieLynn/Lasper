use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::app::AppData;
use super::super::core::utils::empty_block;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Logs "), area);
        return;
    }

    let lines: Vec<Line> = if data.log_lines.is_empty() {
        vec![Line::from(Span::styled(
            "No log output.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        data.log_lines
            .iter()
            .map(|l| Line::from(Span::raw(l.as_str())))
            .collect()
    };

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}
