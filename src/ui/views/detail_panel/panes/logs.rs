use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use super::super::DetailPanel;
use crate::app::AppData;
use crate::ui::theme;

pub fn render(f: &mut Frame, data: &AppData, panel: &DetailPanel, area: Rect) {
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

    let scroll_y = panel.log_scroll as usize;
    let cache = &panel.log_cache;

    let first_line_idx = match cache.offset_index.binary_search(&scroll_y) {
        Ok(idx) => idx,
        Err(idx) => idx.saturating_sub(1),
    };

    let first_line_start_y = cache
        .offset_index
        .get(first_line_idx)
        .copied()
        .unwrap_or(0);
    let skip_visual_lines = scroll_y.saturating_sub(first_line_start_y);

    // Convert String lines to ratatui Lines for rendering
    let visible_lines: Vec<Line> = buffer
        .lines
        .iter()
        .skip(first_line_idx)
        .take(500)
        .map(|s| Line::from(Span::raw(s.clone())))
        .collect();

    f.render_widget(
        Paragraph::new(visible_lines)
            .wrap(Wrap { trim: false })
            .scroll((skip_visual_lines as u16, 0)),
        area,
    );
}
