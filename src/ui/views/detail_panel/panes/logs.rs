use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use crate::app::AppData;
use crate::ui::theme;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Logs "), area);
        return;
    }

    let buffer = match data.log_manager.active_buffer() {
        Some(b) => b,
        None => {
            f.render_widget(
                Paragraph::new(vec![Line::from(Span::styled(
                    "No log output.",
                    Style::default().fg(theme::theme().text_secondary),
                ))]),
                area,
            );
            return;
        }
    };

    if buffer.lines.is_empty() {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "No log output.",
                Style::default().fg(theme::theme().text_secondary),
            ))]),
            area,
        );
        return;
    }

    let scroll_y = scroll as usize;
    let first_line_idx = match buffer.offset_index.binary_search(&scroll_y) {
        Ok(idx) => idx,
        Err(idx) => idx.saturating_sub(1),
    };

    let first_line_start_y = buffer
        .offset_index
        .get(first_line_idx)
        .copied()
        .unwrap_or(0);
    let skip_visual_lines = scroll_y.saturating_sub(first_line_start_y);

    let mut visible_lines = Vec::new();
    for i in first_line_idx..buffer.lines.len() {
        let line = &buffer.lines[i];
        visible_lines.push(line.clone());

        if visible_lines.len() > 500 {
            break;
        }
    }

    f.render_widget(
        Paragraph::new(visible_lines)
            .wrap(Wrap { trim: false })
            .scroll((skip_visual_lines as u16, 0)),
        area,
    );
}
