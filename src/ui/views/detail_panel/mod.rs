pub mod core;
pub mod panes;

pub use core::style::property_style;
pub use core::utils::{detail_block, empty_block};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear,
    },
    Frame,
};

use crate::app::{AppData, DetailPane};
use crate::ui::core::{AppMessage, ContainerMessage, EventResult};
use crate::{handle_nav};

pub struct DetailPanel {
    pub active_pane: DetailPane,
    pub details_scroll: u16,
    pub properties_scroll: u16,
    pub log_scroll: u16,
    pub config_scroll: u16,
    pub pane_height: u16,
    pub focused: bool,
    pub(crate) details_len: usize,
    pub(crate) properties_len: usize,
    pub(crate) logs_len: usize,
    pub(crate) config_len: usize,
}

impl DetailPanel {
    pub fn new() -> Self {
        Self {
            active_pane: DetailPane::Properties,
            details_scroll: 0,
            properties_scroll: 0,
            log_scroll: 0,
            config_scroll: 0,
            pane_height: 10,
            focused: false,
            details_len: 0,
            properties_len: 0,
            logs_len: 0,
            config_len: 0,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
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
        let pane_width = inner_area.width as usize;
        
        // Use extracted scroll logic
        core::scrolling::sync_data_lengths(self, data, pane_width.max(1));

        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Render content area directly in inner_area
        match self.active_pane {
            DetailPane::Properties => {
                panes::properties::render(f, data, inner_area, self.properties_scroll)
            }
            DetailPane::Details => panes::details::render(f, data, inner_area, self.details_scroll),
            DetailPane::Logs => panes::logs::render(f, data, inner_area, self.log_scroll),
            DetailPane::Config => panes::configs::render(f, data, inner_area, self.config_scroll),
            DetailPane::Metrics => panes::metrics::render(f, data, inner_area),
        }

        // Render scrollbar via extracted logic
        core::scrolling::render_scrollbar(self, f, area);
    }

    fn get_tabs_line(&self, data: &AppData) -> Line<'static> {
        let selected = match self.active_pane {
            DetailPane::Properties => 0,
            DetailPane::Details => 1,
            DetailPane::Logs => 2,
            DetailPane::Config => 3,
            DetailPane::Metrics => 4,
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

        let labels = [
            " Properties ",
            " Details ",
            log_label,
            " Config ",
            " Metrics ",
        ];

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
                self.properties_scroll = 0;
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Properties,
                )));
            }
            KeyCode::Char('d') => {
                self.active_pane = DetailPane::Details;
                self.details_scroll = 0;
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
            KeyCode::Char('m') => {
                self.active_pane = DetailPane::Metrics;
                return EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                    DetailPane::Metrics,
                )));
            }

            // ─── Detail scrolling ──────────────────────────────────────────
            _ if self.active_pane == DetailPane::Logs => {
                handle_nav!(self, log_scroll, self.logs_len, step, self.pane_height, key);
            }
            _ if self.active_pane == DetailPane::Config => {
                handle_nav!(self, config_scroll, self.config_len, step, self.pane_height, key);
            }
            _ if self.active_pane == DetailPane::Details => {
                handle_nav!(self, details_scroll, self.details_len, step, self.pane_height, key);
            }
            _ if self.active_pane == DetailPane::Properties => {
                handle_nav!(self, properties_scroll, self.properties_len, step, self.pane_height, key);
            }

            _ => {}
        }
        EventResult::Ignored
    }
}
