use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use crate::app::AppData;
use super::empty_block;

pub fn render(
    f: &mut Frame,
    data: &AppData,
    area: Rect,
    state: &mut ratatui::widgets::TableState,
) {
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
            f.render_widget(
                Paragraph::new(error_text)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };

    let mut pairs: Vec<(&String, &String)> = props.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());

    let rows: Vec<Row> = pairs
        .iter()
        .map(|(k, v)| {
            Row::new(vec![
                Cell::from(k.as_str()).style(Style::default().fg(Color::Cyan)),
                Cell::from(v.as_str()),
            ])
        })
        .collect();

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
