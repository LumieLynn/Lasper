use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use crate::tui::app::AppData;
use crate::tui::theme;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.detail_target.name().is_none() {
        f.render_widget(empty_block(" Config "), area);
        return;
    }

    let t = theme::theme();
    let mut lines = Vec::new();
    if let Some(path) = &data.config_path {
        lines.push(Line::from(vec![
            Span::styled("Source = ", Style::default().fg(t.config_key)),
            Span::styled(
                path.display().to_string(),
                Style::default().fg(t.config_value),
            ),
        ]));
        lines.push(Line::from(""));
    }
    if let Some(text) = &data.config_content {
        lines.extend(crate::tui::widgets::display::config_text::highlighted_lines(text));
    } else if let Some(error) = &data.config_error {
        lines.push(Line::from(Span::styled(
            format!("Configuration unavailable: {error}"),
            Style::default().fg(t.error),
        )));
    } else {
        let name = data.detail_target.name().unwrap_or("?");
        lines.push(Line::from(format!(
            "No .nspawn config file found for machine '{}'.",
            name
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}
