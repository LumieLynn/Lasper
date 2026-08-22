use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
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
        lines.extend(text.lines().map(|l| {
            if l.starts_with('[') && l.ends_with(']') {
                Line::from(Span::styled(
                    l.to_owned(),
                    Style::default()
                        .fg(t.config_section)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if let Some(pos) = l.find('=') {
                let (k, v) = l.split_at(pos);
                Line::from(vec![
                    Span::styled(k.to_owned(), Style::default().fg(t.config_key)),
                    Span::styled(v.to_owned(), Style::default().fg(t.config_value)),
                ])
            } else {
                Line::from(Span::styled(
                    l.to_owned(),
                    Style::default().fg(t.text_secondary),
                ))
            }
        }));
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
