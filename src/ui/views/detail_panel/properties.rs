use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use crate::app::AppData;
use super::empty_block;

const IMPORTANT_KEYS: &[&str] = &[
    "Name",
    "State",
    "Class",
    "Enabled",
    "IPAddresses",
    "MainPID",
    "Leader",
    "Timestamp",
    "Type",
    "ReadOnly",
    "Usage",
];

fn property_style(key: &str, value: &str) -> Style {
    match key {
        "Enabled" => match value {
            "enabled" | "enabled-runtime" => Style::default().fg(Color::Green),
            "disabled" => Style::default().fg(Color::Red),
            _ => Style::default().fg(Color::Yellow),
        },
        "State" => match value {
            "running" => Style::default().fg(Color::Green),
            "poweroff" => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Yellow),
        },
        _ => Style::default().fg(Color::White),
    }
}

pub fn render(f: &mut Frame, data: &AppData, area: Rect) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Properties "), area);
        return;
    }

    let props = match &data.properties {
        Ok(p) => p,
        Err(e) => {
            let error_text = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Error: ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(e.clone(), Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };

    let mut pairs: Vec<(&String, &String)> = props
        .iter()
        .filter(|(k, _)| IMPORTANT_KEYS.contains(&k.as_str()))
        .collect();

    pairs.sort_by_key(|(k, _)| {
        IMPORTANT_KEYS
            .iter()
            .position(|&ik| ik == k.as_str())
            .unwrap_or(usize::MAX)
    });

    let rows: Vec<Row> = pairs
        .iter()
        .map(|(k, v)| {
            let val_style = property_style(k.as_str(), v.as_str());
            Row::new(vec![
                Cell::from(k.as_str()).style(Style::default().fg(Color::Cyan)),
                Cell::from(v.as_str()).style(val_style),
            ])
        })
        .collect();

    let widths = [Constraint::Percentage(35), Constraint::Percentage(65)];
    let table = Table::new(rows, widths)
        .row_highlight_style(Style::default().fg(Color::Cyan));

    f.render_widget(table, area);
}
