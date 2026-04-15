use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use super::empty_block;
use super::properties::property_style;
use crate::app::AppData;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, state: &mut ratatui::widgets::TableState) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" All Details "), area);
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
            f.render_widget(Paragraph::new(error_text).wrap(Wrap { trim: false }), area);
            return;
        }
    };

    let val_width = if area.width > 6 {
        (area.width as f32 * 0.65) as usize
    } else {
        10
    };
    let val_width = val_width.saturating_sub(3).max(10); // buffer for table spacing

    let mut rows = Vec::new();

    // Sort groups for consistent display order
    let mut sorted_groups = props.groups.clone();
    sorted_groups.sort_by_key(|g| g.display_priority());

    for group in &sorted_groups {
        if group.properties.is_empty() {
            continue;
        }

        // Add Header Row
        rows.push(
            Row::new(vec![
                Cell::from(format!("[ {} ]", group.name.to_uppercase()))
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Cell::from(""),
            ])
            .height(1),
        );

        let mut pairs: Vec<(&String, &String)> = group.properties.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());

        for (k, v) in pairs {
            let val_style = property_style(k, v);
            let mut wrapped_lines = Vec::new();
            for line in v.split('\n') {
                if line.is_empty() {
                    wrapped_lines.push(Line::from(""));
                    continue;
                }
                let mut current = String::new();
                let mut width = 0;
                for c in line.chars() {
                    if width >= val_width {
                        wrapped_lines.push(Line::from(current));
                        current = String::new();
                        width = 0;
                    }
                    current.push(c);
                    width += 1;
                }
                if !current.is_empty() {
                    wrapped_lines.push(Line::from(current));
                }
            }
            if wrapped_lines.is_empty() {
                wrapped_lines.push(Line::from(""));
            }
            let height = wrapped_lines.len() as u16;

            rows.push(
                Row::new(vec![
                    Cell::from(k.as_str()).style(Style::default().fg(Color::Cyan)),
                    Cell::from(wrapped_lines).style(val_style),
                ])
                .height(height),
            );
        }

        // Add Spacer
        rows.push(Row::new(vec![Cell::from(""), Cell::from("")]).height(1));
    }

    let widths = [Constraint::Percentage(35), Constraint::Percentage(65)];
    let table = Table::new(rows, widths)
        .highlight_symbol(">> ")
        .row_highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, state);
}
