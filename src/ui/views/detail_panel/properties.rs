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

pub fn property_style(key: &str, value: &str) -> Style {
    // 1) Global Semantic Matching (matches our unified formatting)
    if value == "yes" && key != "ReadOnly" {
        return Style::default().fg(Color::Green);
    }
    if value == "no" {
        return Style::default().fg(Color::DarkGray);
    }

    match key {
        "Enabled" => match value {
            "enabled" | "enabled-runtime" | "yes" => Style::default().fg(Color::Green),
            "disabled" | "no" => Style::default().fg(Color::Red),
            _ => Style::default().fg(Color::Yellow),
        },
        "State" => match value {
            "running" | "yes" => Style::default().fg(Color::Green),
            "starting" | "exiting" => Style::default().fg(Color::Cyan).add_modifier(Modifier::ITALIC),
            "poweroff" | "no" => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Yellow),
        },
        "ReadOnly" => {
            if value == "yes" {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        }
        "MainPID" | "Leader" => Style::default().fg(Color::Magenta),
        "MemoryCurrent" | "Usage" => Style::default().fg(Color::Blue),
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

    let mut pairs = Vec::new();
    for group in &props.groups {
        for (k, v) in &group.properties {
            if IMPORTANT_KEYS.contains(&k.as_str()) {
                pairs.push((k, v));
            }
        }
    }

    // Deduplicate keys (keeping whichever one appeared first)
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(k, _)| seen.insert(k.as_str()));

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
