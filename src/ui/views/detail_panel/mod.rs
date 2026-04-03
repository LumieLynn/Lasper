pub mod configs;
pub mod details;
pub mod logs;
pub mod properties;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::app::{AppData, DetailPane};
use crate::ui::core::{AppMessage, ContainerMessage, EventResult};

pub struct DetailPanel {
    pub active_pane: DetailPane,
    pub details_state: ratatui::widgets::TableState,
    pub log_scroll: u16,
    pub config_scroll: u16,
    pub pane_height: u16,
    pub focused: bool,
    details_len: usize,
    logs_len: usize,
    config_len: usize,
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
            logs_len: 0,
            config_len: 0,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn sync_data_lengths(&mut self, data: &AppData) {
        let old_logs_len = self.logs_len;
        self.details_len = data.properties.as_ref().map(|p| p.len()).unwrap_or(0);
        self.logs_len = data.log_lines.len();
        self.config_len = data
            .config_content
            .as_ref()
            .map(|c| c.lines().count())
            .unwrap_or(0);

        // Sticky autoscroll for Logs
        if self.active_pane == DetailPane::Logs && self.logs_len > old_logs_len {
            let max_scroll_old = old_logs_len.saturating_sub(self.pane_height as usize) as u16;
            if self.log_scroll >= max_scroll_old {
                let max_scroll_new =
                    self.logs_len.saturating_sub(self.pane_height as usize);
                self.log_scroll = max_scroll_new.min(u16::MAX as usize) as u16;
            }
        }
    }

    pub fn render_with_data(&mut self, f: &mut Frame, area: Rect, data: &AppData) {

        // Border
        let border_color = if self.focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let tabs_line = self.get_tabs_line(data);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(tabs_line);

        // Get inner area
        let inner_area = block.inner(area);
        self.pane_height = inner_area.height;
        self.sync_data_lengths(data);

        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Render content area directly in inner_area
        match self.active_pane {
            DetailPane::Properties => properties::render(f, data, inner_area),
            DetailPane::Details => details::render(f, data, inner_area, &mut self.details_state),
            DetailPane::Logs => logs::render(f, data, inner_area, self.log_scroll),
            DetailPane::Config => configs::render(f, data, inner_area, self.config_scroll),
        }

        // Render scrollbar
        self.render_scrollbar(f, area);
    }

    fn render_scrollbar(&mut self, f: &mut Frame, area: Rect) {
        let (max_scroll, position) = match self.active_pane {
            DetailPane::Logs if self.logs_len > self.pane_height as usize => {
                (
                    self.logs_len.saturating_sub(self.pane_height as usize),
                    self.log_scroll as usize,
                )
            }
            DetailPane::Config if self.config_len > self.pane_height as usize => {
                (
                    self.config_len.saturating_sub(self.pane_height as usize),
                    self.config_scroll as usize,
                )
            }
            DetailPane::Details if self.details_len > 1 => (
                self.details_len.saturating_sub(1),
                self.details_state.selected().unwrap_or(0),
            ),
            _ => return,
        };

        let mut state = ScrollbarState::new(max_scroll).position(position);

        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));

        let scrollbar_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(2),
        };

        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }

    fn get_tabs_line(&self, data: &AppData) -> Line<'static> {
        let selected = match self.active_pane {
            DetailPane::Properties => 0,
            DetailPane::Details => 1,
            DetailPane::Logs => 2,
            DetailPane::Config => 3,
        };

        let stopped = data.entries.is_empty()
            || data
                .entries
                .get(data.selected)
                .map(|e| !e.state.is_running())
                .unwrap_or(true);

        let log_label = if stopped {
            " Logs (poweroff) "
        } else {
            " Logs "
        };

        let labels = [" Properties ", " Details ", log_label, " Config "];

        let mut spans = Vec::new();

        for (i, label) in labels.iter().enumerate() {
            let mut style = Style::default().fg(Color::DarkGray);
            if i == selected {
                style = style
                    .fg(if self.focused {
                        Color::Yellow
                    } else {
                        Color::White
                    })
                    .add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled((*label).to_string(), style));

            if i < labels.len() - 1 {
                spans.push(Span::raw("-"));
            }
        }
        Line::from(spans)
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
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Properties,
                )));
            }
            KeyCode::Char('d') => {
                self.active_pane = DetailPane::Details;
                self.details_state.select(Some(0));
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Details,
                )));
            }
            KeyCode::Char('l') => {
                self.active_pane = DetailPane::Logs;
                let max = self.logs_len.saturating_sub(self.pane_height as usize);
                self.log_scroll = max.min(u16::MAX as usize) as u16;
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Logs,
                )));
            }
            KeyCode::Char('c') => {
                self.active_pane = DetailPane::Config;
                self.config_scroll = 0;
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Config,
                )));
            }

            // ─── Logs scrolling ────────────────────────────────────────────
            KeyCode::Up if self.active_pane == DetailPane::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Logs => {
                let max = self.logs_len.saturating_sub(self.pane_height as usize);
                let safe_max = max.min(u16::MAX as usize) as u16;
                self.log_scroll = (self.log_scroll + 1).min(safe_max);
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(step);
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Logs => {
                let max = self.logs_len.saturating_sub(self.pane_height as usize);
                let safe_max = max.min(u16::MAX as usize) as u16;
                self.log_scroll = (self.log_scroll + step).min(safe_max);
                return EventResult::Consumed;
            }

            // ─── Config scrolling ──────────────────────────────────────────
            KeyCode::Up if self.active_pane == DetailPane::Config => {
                self.config_scroll = self.config_scroll.saturating_sub(1);
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Config => {
                let max = self.config_len.saturating_sub(self.pane_height as usize);
                let safe_max = max.min(u16::MAX as usize) as u16;
                self.config_scroll = (self.config_scroll + 1).min(safe_max);
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Config => {
                self.config_scroll = self.config_scroll.saturating_sub(step);
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Config => {
                let max = self.config_len.saturating_sub(self.pane_height as usize);
                let safe_max = max.min(u16::MAX as usize) as u16;
                self.config_scroll = (self.config_scroll + step).min(safe_max);
                return EventResult::Consumed;
            }

            // ─── Details table navigation (clamped by cached details_len) ──
            KeyCode::Up if self.active_pane == DetailPane::Details => {
                let i = self
                    .details_state
                    .selected()
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::Down if self.active_pane == DetailPane::Details => {
                let max = self.details_len.saturating_sub(1);
                let i = self
                    .details_state
                    .selected()
                    .map(|i| (i + 1).min(max))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::PageUp if self.active_pane == DetailPane::Details => {
                let i = self
                    .details_state
                    .selected()
                    .map(|i| i.saturating_sub(step as usize))
                    .unwrap_or(0);
                self.details_state.select(Some(i));
                return EventResult::Consumed;
            }
            KeyCode::PageDown if self.active_pane == DetailPane::Details => {
                let max = self.details_len.saturating_sub(1);
                let i = self
                    .details_state
                    .selected()
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

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn detail_block(_title: &str) -> Block<'_> {
    Block::default().style(Style::default().fg(Color::White))
}

pub(crate) fn empty_block(title: &str) -> Paragraph<'_> {
    Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(detail_block(title))
}
