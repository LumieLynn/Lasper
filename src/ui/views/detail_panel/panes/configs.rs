use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use crate::app::AppData;
use crate::ui::theme;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Config "), area);
        return;
    }

    let t = theme::theme();
    let text = match &data.config_content {
        Some(c) => c.clone(),
        None => {
            let name = data
                .entries
                .get(data.selected)
                .map(|e| e.name.as_str())
                .unwrap_or("?");
            format!("No .nspawn config file found for machine '{}'.", name)
        }
    };

    let lines: Vec<Line> = text
        .lines()
        .map(|l| {
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
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}
