use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap},
    Frame,
};

use crate::app::{AppData, DetailPane};
use crate::ui::core::{AppMessage, EventResult};

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

// ── PropertyFormatter ─────────────────────────────────────────────────────────

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

// ── DetailPanel ───────────────────────────────────────────────────────────────

pub struct DetailPanel {
    pub active_pane: DetailPane,
    pub details_state: ratatui::widgets::TableState,
    pub log_scroll: u16,
    pub config_scroll: u16,
    /// Height of the scrollable pane area — set by layout before handling keys.
    pub pane_height: u16,
    pub focused: bool,
    /// Cached row count for the Details pane. Updated in render_with_data.
    /// Used by handle_key to clamp selection without touching state during render.
    details_len: usize,
}

impl DetailPanel {
    pub fn new() -> Self {
        Self {
            active_pane: DetailPane::Properties,
            details_state: ratatui::widgets::TableState::default(),
            log_scroll: 0,
            config_scroll: 0,
            pane_height: 10,
            focused: false,
            details_len: 0,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Call this from layout / render_with_data before handle_key so
    /// the Details pane length is always in sync with live data.
    fn sync_details_len(&mut self, data: &AppData) {
        self.details_len = data.properties.as_ref().map(|p| p.len()).unwrap_or(0);
    }

    pub fn render_with_data(&mut self, f: &mut Frame, area: Rect, data: &AppData) {
        self.sync_details_len(data);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);

        render_sub_tabs(f, &self.active_pane, data, self.focused, chunks[0]);
        self.render_pane(f, data, chunks[1]);
    }

    fn render_pane(&mut self, f: &mut Frame, data: &AppData, area: Rect) {
        match self.active_pane {
            DetailPane::Properties => render_properties(f, data, area),
            DetailPane::Details    => render_full_details(f, data, area, &mut self.details_state),
            DetailPane::Logs       => render_logs(f, data, area, self.log_scroll),
            DetailPane::Config     => render_config(f, data, area, self.config_scroll),
        }
    }

    fn page_step(&self) -> u16 {
        (self.pane_height / 2).max(1)
    }

    /// Handles all keyboard input for the detail panel.
    /// Returns Consumed for scroll/navigation, Message for pane switches.
    pub fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let step = self.page_step();

        match key.code {
            // ─── Pane switching ────────────────────────────────────────────
            KeyCode::Char('p') => {
                self.active_pane = DetailPane::Properties;
                return EventResult::Message(AppMessage::DetailPaneChanged(DetailPane::Properties));
            }
            KeyCode::Char('d') => {
                self.active_pane = DetailPane::Details;
                self.details_state.select(Some(0));
                return EventResult::Message(AppMessage::DetailPaneChanged(DetailPane::Details));
            }
            KeyCode::Char('l') => {
                self.active_pane = DetailPane::Logs;
                self.log_scroll = 0;
                return EventResult::Message(AppMessage::DetailPaneChanged(DetailPane::Logs));
            }
            KeyCode::Char('c') => {
                self.active_pane = DetailPane::Config;
                self.config_scroll = 0;
                return EventResult::Message(AppMessage::DetailPaneChanged(DetailPane::Config));
            }

            // ─── Logs scrolling ────────────────────────────────────────────
            KeyCode::Up if self.active_pane == DetailPane::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Logs => {
                self.log_scroll += 1;
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(step);
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Logs => {
                self.log_scroll += step;
                return EventResult::Consumed;
            }

            // ─── Config scrolling ──────────────────────────────────────────
            KeyCode::Up if self.active_pane == DetailPane::Config => {
                self.config_scroll = self.config_scroll.saturating_sub(1);
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Config => {
                self.config_scroll += 1;
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Config => {
                self.config_scroll = self.config_scroll.saturating_sub(step);
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Config => {
                self.config_scroll += step;
                return EventResult::Consumed;
            }

            // ─── Details table navigation (clamped by cached details_len) ──
            KeyCode::Up if self.active_pane == DetailPane::Details => {
                let i = self.details_state.selected()
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Details => {
                let max = self.details_len.saturating_sub(1);
                let i = self.details_state.selected()
                    .map(|i| (i + 1).min(max))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Details => {
                let i = self.details_state.selected()
                    .map(|i| i.saturating_sub(step as usize))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Details => {
                let max = self.details_len.saturating_sub(1);
                let i = self.details_state.selected()
                    .map(|i| (i + step as usize).min(max))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }

            _ => {}
        }
        EventResult::Ignored
    }
}

// ── Sub-tabs ──────────────────────────────────────────────────────────────────

fn render_sub_tabs(
    f: &mut Frame,
    active_pane: &DetailPane,
    data: &AppData,
    focused: bool,
    area: Rect,
) {
    let selected = match active_pane {
        DetailPane::Properties => 0,
        DetailPane::Details    => 1,
        DetailPane::Logs       => 2,
        DetailPane::Config     => 3,
    };

    let stopped = data.entries.is_empty()
        || data.entries.get(data.selected)
            .map(|e| !e.state.is_running())
            .unwrap_or(true);
    let log_label = if stopped { " Logs (poweroff) " } else { " Logs " };

    let titles = vec![
        Line::from(" Properties "),
        Line::from(" Details "),
        Line::from(log_label),
        Line::from(" Config "),
    ];

    let highlight_color = if focused { Color::Cyan } else { Color::DarkGray };

    let tabs = Tabs::new(titles)
        .select(selected)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(highlight_color)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    f.render_widget(tabs, area);
}

// ── Properties ────────────────────────────────────────────────────────────────

fn render_properties(f: &mut Frame, data: &AppData, area: Rect) {
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
                    Span::styled("  Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled(e.clone(), Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text)
                    .block(detail_block(" Properties (Summary) "))
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
        .block(
            Block::default()
                .title(" Properties (Summary) ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        )
        .row_highlight_style(Style::default().fg(Color::Cyan));

    f.render_widget(table, area);
}

// ── Full Details ──────────────────────────────────────────────────────────────

fn render_full_details(
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
                    Span::styled("  Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled(e.clone(), Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text)
                    .block(detail_block(" Full Details (Scroll with ↑/↓) "))
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };

    let mut pairs: Vec<(&String, &String)> = props.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());

    // Render is now a pure function — no state mutation here.
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

    f.render_stateful_widget(table, area, state);
}

// ── Logs ──────────────────────────────────────────────────────────────────────

fn render_logs(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Logs "), area);
        return;
    }

    let lines: Vec<Line> = if data.log_lines.is_empty() {
        vec![Line::from(Span::styled(
            "No log output.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        data.log_lines
            .iter()
            .map(|l| Line::from(Span::raw(l.as_str())))
            .collect()
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(detail_block(" Logs "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

// ── Config ────────────────────────────────────────────────────────────────────

fn render_config(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Config "), area);
        return;
    }

    let text = match &data.config_content {
        Some(c) => c.clone(),
        None => {
            let name = data.entries.get(data.selected).map(|e| e.name.as_str()).unwrap_or("?");
            format!("No config file found at /etc/systemd/nspawn/{}.nspawn", name)
        }
    };

    let lines: Vec<Line> = text
        .lines()
        .map(|l| {
            if l.starts_with('[') && l.ends_with(']') {
                Line::from(Span::styled(
                    l.to_owned(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ))
            } else if let Some(pos) = l.find('=') {
                let (k, v) = l.split_at(pos);
                Line::from(vec![
                    Span::styled(k.to_owned(), Style::default().fg(Color::Yellow)),
                    Span::styled(v.to_owned(), Style::default().fg(Color::White)),
                ])
            } else {
                Line::from(Span::styled(l.to_owned(), Style::default().fg(Color::DarkGray)))
            }
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .block(detail_block(" Config "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn detail_block(title: &str) -> Block<'_> {
    Block::default()
        .title(title.to_owned())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(Color::White))
}

fn empty_block(title: &str) -> Paragraph<'_> {
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(detail_block(title))
}
