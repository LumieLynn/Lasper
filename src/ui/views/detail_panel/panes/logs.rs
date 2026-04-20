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

    if data.log_lines.is_empty() {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "No log output.",
                Style::default().fg(Color::DarkGray),
            ))]),
            area,
        );
        return;
    }

    // Binary search to find the first logical line that is visible at the current scroll
    let scroll_y = scroll as usize;
    let first_line_idx = match data.log_offset_index.binary_search(&scroll_y) {
        Ok(idx) => idx,
        Err(idx) => idx.saturating_sub(1),
    };

    // Calculate how many visual lines into the first visible logical line we are
    let first_line_start_y = data.log_offset_index.get(first_line_idx).copied().unwrap_or(0);
    let skip_visual_lines = scroll_y.saturating_sub(first_line_start_y);

    // Collect only enough lines to fill the viewport plus a small buffer
    let mut visible_lines = Vec::new();

    // Collect all remaining lines from the start index to ensure the bottom is never cut off
    for i in first_line_idx..data.log_lines.len() {
        let line = &data.log_lines[i];
        visible_lines.push(line.clone());
        
        // Use a loose limit for performance, but 500 lines is safe
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
