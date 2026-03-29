use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap},
    Frame,
};

use crate::app::{App, DetailPane};

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

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_sub_tabs(f, app, chunks[0]);
    render_pane(f, app, chunks[1]);
}

fn render_sub_tabs(f: &mut Frame, app: &App, area: Rect) {
    let selected = match app.ui.detail_pane {
        DetailPane::Properties => 0,
        DetailPane::Details => 1,
        DetailPane::Logs => 2,
        DetailPane::Config => 3,
    };

    // Dim logs tab if container is stopped
    let stopped = app
        .selected_entry()
        .map(|e| !e.state.is_running())
        .unwrap_or(true);
    let log_label = if stopped {
        " Logs (poweroff) "
    } else {
        " Logs "
    };

    let titles = vec![
        Line::from(" Properties "),
        Line::from(" Details "),
        Line::from(log_label),
        Line::from(" Config "),
    ];

    let tabs = Tabs::new(titles)
        .select(selected)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    f.render_widget(tabs, area);
}

fn render_pane(f: &mut Frame, app: &mut App, area: Rect) {
    match app.ui.detail_pane {
        DetailPane::Properties => render_properties(f, app, area),
        DetailPane::Details => render_full_details(f, app, area),
        DetailPane::Logs => render_logs(f, app, area),
        DetailPane::Config => render_config(f, app, area),
    }
}

// ── Properties ────────────────────────────────────────────────────────────────

fn render_properties(f: &mut Frame, app: &App, area: Rect) {
    if app.entries.is_empty() {
        f.render_widget(empty_block(" Properties "), area);
        return;
    }

    let props = match &app.properties {
        Ok(p) => p,
        Err(e) => {
            let error_text = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled(e, Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text).block(detail_block(" Properties (Summary) ")).wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };

    let mut pairs: Vec<(&String, &String)> = props
        .iter()
        .filter(|(k, _)| IMPORTANT_KEYS.contains(&k.as_str()))
        .collect();

    // Sort pairs by IMPORTANT_KEYS order
    pairs.sort_by_key(|(k, _)| {
        IMPORTANT_KEYS
            .iter()
            .position(|&ik| ik == k.as_str())
            .unwrap_or(usize::MAX)
    });

    let rows: Vec<Row> = pairs
        .iter()
        .map(|(k, v)| {
            let val_style = if k.as_str() == "Enabled" {
                match v.as_str() {
                    "enabled" | "enabled-runtime" => Style::default().fg(Color::Green),
                    "disabled" => Style::default().fg(Color::Red),
                    _ => Style::default().fg(Color::Yellow),
                }
            } else if k.as_str() == "State" {
                match v.as_str() {
                    "running" => Style::default().fg(Color::Green),
                    "poweroff" => Style::default().fg(Color::DarkGray),
                    _ => Style::default().fg(Color::Yellow),
                }
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                Cell::from(k.as_str()).style(Style::default().fg(Color::Cyan)),
                Cell::from(v.as_str()).style(val_style),
            ])
        })
        .collect();

    let widths = [Constraint::Percentage(35), Constraint::Percentage(65)];

    let table = Table::new(rows, widths)
        .block(
            Block::default()
                .title(" Properties (Summary) ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        )
        .row_highlight_style(Style::default().fg(Color::Cyan));

    f.render_widget(table, area);
}

fn render_full_details(f: &mut Frame, app: &mut App, area: Rect) {
    if app.entries.is_empty() {
        f.render_widget(empty_block(" All Details "), area);
        return;
    }

    let props = match &app.properties {
        Ok(p) => p,
        Err(e) => {
            let error_text = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled(e, Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text).block(detail_block(" Full Details (Scroll with ↑/↓) ")).wrap(Wrap { trim: false }),
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
        .block(
            Block::default()
                .title(" Full Details (Scroll with ↑/↓) ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        )
        .highlight_symbol(">> ")
        .row_highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    f.render_stateful_widget(table, area, &mut app.ui.details_state);
}

// ── Logs ──────────────────────────────────────────────────────────────────────

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    if app.entries.is_empty() {
        f.render_widget(empty_block(" Logs "), area);
        return;
    }

    let lines: Vec<Line> = if app.log_lines.is_empty() {
        vec![Line::from(Span::styled(
            "No log output.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.log_lines
            .iter()
            .map(|l| Line::from(Span::raw(l.as_str())))
            .collect()
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(detail_block(" Logs "))
            .wrap(Wrap { trim: false })
            .scroll((app.ui.log_scroll, 0)),
        area,
    );
}

// ── Config ────────────────────────────────────────────────────────────────────

fn render_config(f: &mut Frame, app: &App, area: Rect) {
    if app.entries.is_empty() {
        f.render_widget(empty_block(" Config "), area);
        return;
    }

    let text = match &app.config_content {
        Some(c) => c.clone(),
        Option::None => {
            let name = app.selected_entry().map(|e| e.name.as_str()).unwrap_or("?");
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
                    l,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if let Some(pos) = l.find('=') {
                let (k, v) = l.split_at(pos);
                Line::from(vec![
                    Span::styled(k, Style::default().fg(Color::Yellow)),
                    Span::styled(v, Style::default().fg(Color::White)),
                ])
            } else {
                Line::from(Span::styled(l, Style::default().fg(Color::DarkGray)))
            }
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .block(detail_block(" Config "))
            .wrap(Wrap { trim: false })
            .scroll((app.ui.config_scroll, 0)),
        area,
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn detail_block(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(Color::White))
}

fn empty_block(title: &'static str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(detail_block(title))
}
