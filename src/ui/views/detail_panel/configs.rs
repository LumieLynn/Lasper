use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::app::AppData;
use super::empty_block;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Config "), area);
        return;
    }

    let text = match &data.config_content {
        Some(c) => c.clone(),
        None => {
            let name = data
                .entries
                .get(data.selected)
                .map(|e| e.name.as_str())
                .unwrap_or("?");
            format!(
                "No config file found at /etc/systemd/nspawn/{}.nspawn",
                name
            )
        }
    };

    let lines: Vec<Line> = text
        .lines()
        .map(|l| {
            if l.starts_with('[') && l.ends_with(']') {
                Line::from(Span::styled(
                    l.to_owned(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if let Some(pos) = l.find('=') {
                let (k, v) = l.split_at(pos);
                Line::from(vec![
                    Span::styled(k.to_owned(), Style::default().fg(Color::Yellow)),
                    Span::styled(v.to_owned(), Style::default().fg(Color::White)),
                ])
            } else {
                Line::from(Span::styled(
                    l.to_owned(),
                    Style::default().fg(Color::DarkGray),
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
