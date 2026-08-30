pub mod core;
pub mod log_manager;
pub mod panes;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

use crate::handle_nav;
use crate::tui::app::AppData;
use crate::tui::core::{AppMessage, ContainerMessage, EventResult};
use crate::tui::views::title_tabs::{
    bordered_title_tab_hitboxes, clicked_title_tab, TitleTabHitbox,
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DetailTarget {
    #[default]
    Empty,
    Machine(String),
    Image {
        name: String,
        internal: bool,
    },
}

impl DetailTarget {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Empty => None,
            Self::Machine(name) | Self::Image { name, .. } => Some(name),
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    pub fn is_internal_image(&self) -> bool {
        matches!(self, Self::Image { internal: true, .. })
    }
}

/// The currently active detail pane in the main UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPane {
    Properties,
    Details,
    Logs,
    Config,
    Metrics,
    ImageOverview,
    ImageConfig,
    ImageUnit,
}

impl DetailPane {
    pub const MACHINE: &[DetailPane] = &[
        DetailPane::Properties,
        DetailPane::Details,
        DetailPane::Logs,
        DetailPane::Config,
        DetailPane::Metrics,
    ];

    pub const IMAGE: &[DetailPane] = &[
        DetailPane::ImageOverview,
        DetailPane::ImageConfig,
        DetailPane::ImageUnit,
    ];
    pub const INTERNAL_IMAGE: &[DetailPane] = &[DetailPane::ImageOverview];

    pub fn tabs_for(target: &DetailTarget) -> &'static [DetailPane] {
        if target.is_image() {
            if target.is_internal_image() {
                Self::INTERNAL_IMAGE
            } else {
                Self::IMAGE
            }
        } else {
            Self::MACHINE
        }
    }

    pub fn next_for(&self, target: &DetailTarget) -> Self {
        let tabs = Self::tabs_for(target);
        let idx = tabs.iter().position(|p| p == self).unwrap_or(0);
        tabs[(idx + 1) % tabs.len()]
    }

    pub fn prev_for(&self, target: &DetailTarget) -> Self {
        let tabs = Self::tabs_for(target);
        let idx = tabs.iter().position(|p| p == self).unwrap_or(0);
        tabs[(idx + tabs.len() - 1) % tabs.len()]
    }

    pub fn from_index(idx: usize, target: &DetailTarget) -> Option<Self> {
        Self::tabs_for(target).get(idx).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPaneContext {
    Machine,
    Image,
    InternalImage,
}

impl DetailPaneContext {
    fn for_target(target: &DetailTarget) -> Self {
        match target {
            DetailTarget::Image { internal: true, .. } => Self::InternalImage,
            DetailTarget::Image {
                internal: false, ..
            } => Self::Image,
            DetailTarget::Empty | DetailTarget::Machine(_) => Self::Machine,
        }
    }
}

pub struct DetailPanel {
    active_pane: DetailPane,
    remembered_machine_pane: DetailPane,
    remembered_image_pane: DetailPane,
    pane_context: DetailPaneContext,
    pub details_scroll: u16,
    pub properties_scroll: u16,
    pub log_scroll: u16,
    pub config_scroll: u16,
    pub unit_scroll: u16,
    pub pane_height: u16,
    pub focused: bool,
    pub(crate) old_pane_height: u16,
    pub(crate) details_len: usize,
    pub(crate) properties_len: usize,
    pub(crate) image_overview_len: usize,
    pub(crate) logs_len: usize,
    pub(crate) config_len: usize,
    pub(crate) unit_len: usize,
    pub(crate) last_rendered_width: u16,
    pub(crate) log_cache: core::scrolling::LogRenderCache,
    scroll_area: Rect,
    tab_hitboxes: Vec<TitleTabHitbox<DetailPane>>,
}

impl DetailPanel {
    pub fn new() -> Self {
        Self {
            active_pane: DetailPane::Properties,
            remembered_machine_pane: DetailPane::Properties,
            remembered_image_pane: DetailPane::ImageOverview,
            pane_context: DetailPaneContext::Machine,
            details_scroll: 0,
            properties_scroll: 0,
            log_scroll: 0,
            config_scroll: 0,
            unit_scroll: 0,
            pane_height: 10,
            old_pane_height: 10,
            focused: false,
            details_len: 0,
            properties_len: 0,
            image_overview_len: 0,
            logs_len: 0,
            config_len: 0,
            unit_len: 0,
            last_rendered_width: 0,
            log_cache: core::scrolling::LogRenderCache::new(),
            scroll_area: Rect::default(),
            tab_hitboxes: Vec::new(),
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn active_pane(&self) -> DetailPane {
        self.active_pane
    }

    pub fn render_with_data(
        &mut self,
        f: &mut Frame,
        area: Rect,
        data: &mut AppData,
        resize_mode: bool,
    ) {
        // Border
        let border_color = crate::tui::panel_border_color(resize_mode, self.focused, false);

        self.ensure_pane_for_target(&data.detail_target);
        let labels = Self::tab_labels(data);
        let tabs = DetailPane::tabs_for(&data.detail_target);
        debug_assert_eq!(tabs.len(), labels.len());
        let tab_widths = tabs
            .iter()
            .copied()
            .zip(labels.iter().map(|label| label.width()))
            .collect::<Vec<_>>();
        self.tab_hitboxes = bordered_title_tab_hitboxes(area, Alignment::Left, &tab_widths, 1);
        let tabs_line = self.get_tabs_line(data, &labels);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(tabs_line);

        // Get inner area
        let inner_area = block.inner(area);
        self.scroll_area = inner_area;
        self.pane_height = inner_area.height;

        // Reserve 1 column for the scrollbar to avoid text overlap and wrapping issues
        let pane_width = (inner_area.width as usize).saturating_sub(1).max(1);

        // Use extracted scroll logic
        core::scrolling::sync_data_lengths(self, data, pane_width);
        self.old_pane_height = self.pane_height;

        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Render content area directly in inner_area
        match self.active_pane {
            DetailPane::Properties => {
                panes::properties::render(f, data, inner_area, self.properties_scroll)
            }
            DetailPane::Details => panes::details::render(f, data, inner_area, self.details_scroll),
            DetailPane::Logs => panes::logs::render(f, data, self, inner_area),
            DetailPane::Config => panes::configs::render(f, data, inner_area, self.config_scroll),
            DetailPane::Metrics => panes::metrics::render(f, data, inner_area),
            DetailPane::ImageOverview => {
                panes::image::render_overview(f, data, inner_area, self.details_scroll)
            }
            DetailPane::ImageConfig => {
                panes::configs::render(f, data, inner_area, self.config_scroll)
            }
            DetailPane::ImageUnit => {
                panes::image::render_unit(f, data, inner_area, self.unit_scroll)
            }
        }

        // Render scrollbar via extracted logic
        core::scrolling::render_scrollbar(self, f, area);
    }

    fn tab_labels(data: &AppData) -> Vec<&'static str> {
        if data.detail_target.is_internal_image() {
            vec![" Overview "]
        } else if data.detail_target.is_image() {
            vec![" Overview ", " Config ", " Unit "]
        } else {
            let stopped = data.entries.is_empty()
                || data
                    .entries
                    .get(data.selected)
                    .map(|e| !e.state.accepts_runtime_actions())
                    .unwrap_or(true);
            vec![
                " Properties ",
                " Details ",
                if stopped {
                    " Logs (poweroff) "
                } else {
                    " Logs "
                },
                " Config ",
                " Metrics ",
            ]
        }
    }

    fn get_tabs_line(&self, data: &AppData, labels: &[&'static str]) -> Line<'static> {
        let tabs = DetailPane::tabs_for(&data.detail_target);
        let selected = tabs
            .iter()
            .position(|pane| pane == &self.active_pane)
            .unwrap_or(0);

        let mut spans = Vec::new();

        let t = crate::tui::theme::theme();
        for (i, label) in labels.iter().enumerate() {
            let mut style = Style::default().fg(t.tab_inactive);
            if i == selected {
                style = style
                    .fg(if self.focused {
                        t.tab_active_focused
                    } else {
                        t.tab_active_unfocused
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

    pub fn ensure_pane_for_target(&mut self, target: &DetailTarget) {
        let context = DetailPaneContext::for_target(target);
        if context == self.pane_context && DetailPane::tabs_for(target).contains(&self.active_pane)
        {
            return;
        }

        self.pane_context = context;
        self.active_pane = match context {
            DetailPaneContext::Machine => self.remembered_machine_pane,
            DetailPaneContext::Image => self.remembered_image_pane,
            DetailPaneContext::InternalImage => DetailPane::ImageOverview,
        };
        debug_assert!(DetailPane::tabs_for(target).contains(&self.active_pane));
    }

    fn page_step(&self) -> u16 {
        (self.pane_height / 2).max(1)
    }

    fn switch_pane(&mut self, pane: DetailPane, target: &DetailTarget) -> EventResult {
        debug_assert!(DetailPane::tabs_for(target).contains(&pane));
        let context = DetailPaneContext::for_target(target);
        self.pane_context = context;
        self.active_pane = pane;
        match context {
            DetailPaneContext::Machine => self.remembered_machine_pane = pane,
            DetailPaneContext::Image => self.remembered_image_pane = pane,
            DetailPaneContext::InternalImage => {}
        }
        match self.active_pane {
            DetailPane::Properties => self.properties_scroll = 0,
            DetailPane::Details => self.details_scroll = 0,
            DetailPane::Logs => {
                let max = self.logs_len.saturating_sub(self.pane_height as usize);
                self.log_scroll = max.min(u16::MAX as usize) as u16;
            }
            DetailPane::Config => self.config_scroll = 0,
            DetailPane::ImageConfig => self.config_scroll = 0,
            DetailPane::ImageUnit => self.unit_scroll = 0,
            DetailPane::Metrics => {}
            DetailPane::ImageOverview => self.details_scroll = 0,
        }
        EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(pane)))
    }

    /// Handles all keyboard input for the detail panel.
    /// Returns Consumed for scroll/navigation, Message for pane switches.
    pub fn handle_key(&mut self, key: KeyEvent, target: &DetailTarget) -> EventResult {
        self.ensure_pane_for_target(target);
        let step = self.page_step();

        match key.code {
            // Pane switching
            KeyCode::Char('[') => {
                let next = self.active_pane.prev_for(target);
                return self.switch_pane(next, target);
            }
            KeyCode::Char(']') => {
                let next = self.active_pane.next_for(target);
                return self.switch_pane(next, target);
            }
            KeyCode::Char(c)
                if c.is_ascii_digit()
                    && key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                let idx = (c.to_digit(10).unwrap() as usize).saturating_sub(1);
                if let Some(pane) = DetailPane::from_index(idx, target) {
                    return self.switch_pane(pane, target);
                }
            }

            // Detail scrolling
            _ if self.active_pane == DetailPane::Logs => {
                handle_nav!(self, log_scroll, self.logs_len, step, self.pane_height, key);
            }
            _ if self.active_pane == DetailPane::Config => {
                handle_nav!(
                    self,
                    config_scroll,
                    self.config_len,
                    step,
                    self.pane_height,
                    key
                );
            }
            _ if self.active_pane == DetailPane::ImageConfig => {
                handle_nav!(
                    self,
                    config_scroll,
                    self.config_len,
                    step,
                    self.pane_height,
                    key
                );
            }
            _ if self.active_pane == DetailPane::ImageUnit => {
                handle_nav!(
                    self,
                    unit_scroll,
                    self.unit_len,
                    step,
                    self.pane_height,
                    key
                );
            }
            _ if self.active_pane == DetailPane::ImageOverview => {
                handle_nav!(
                    self,
                    details_scroll,
                    self.image_overview_len,
                    step,
                    self.pane_height,
                    key
                );
            }
            _ if self.active_pane == DetailPane::Details => {
                handle_nav!(
                    self,
                    details_scroll,
                    self.details_len,
                    step,
                    self.pane_height,
                    key
                );
            }
            _ if self.active_pane == DetailPane::Properties => {
                handle_nav!(
                    self,
                    properties_scroll,
                    self.properties_len,
                    step,
                    self.pane_height,
                    key
                );
            }

            _ => {}
        }
        EventResult::Ignored
    }

    /// Switch tabs or scroll the active detail pane.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, target: &DetailTarget) -> EventResult {
        self.ensure_pane_for_target(target);
        if let Some(pane) = clicked_title_tab(&self.tab_hitboxes, mouse) {
            if DetailPane::tabs_for(target).contains(&pane) {
                return self.switch_pane(pane, target);
            }
        }

        if mouse.column < self.scroll_area.x
            || mouse.column >= self.scroll_area.x.saturating_add(self.scroll_area.width)
            || mouse.row < self.scroll_area.y
            || mouse.row >= self.scroll_area.y.saturating_add(self.scroll_area.height)
        {
            return EventResult::Ignored;
        }

        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => Some(3u16),
            MouseEventKind::ScrollDown => Some(3u16),
            _ => None,
        };
        let Some(delta) = delta else {
            return EventResult::Ignored;
        };

        let (scroll, content_len) = match self.active_pane {
            DetailPane::Properties => (&mut self.properties_scroll, self.properties_len),
            DetailPane::ImageOverview => (&mut self.details_scroll, self.image_overview_len),
            DetailPane::ImageConfig => (&mut self.config_scroll, self.config_len),
            DetailPane::ImageUnit => (&mut self.unit_scroll, self.unit_len),
            DetailPane::Details => (&mut self.details_scroll, self.details_len),
            DetailPane::Logs => (&mut self.log_scroll, self.logs_len),
            DetailPane::Config => (&mut self.config_scroll, self.config_len),
            DetailPane::Metrics => return EventResult::Consumed,
        };
        let max = content_len
            .saturating_sub(usize::from(self.pane_height))
            .min(usize::from(u16::MAX)) as u16;
        if matches!(mouse.kind, MouseEventKind::ScrollUp) {
            *scroll = scroll.saturating_sub(delta);
        } else {
            *scroll = scroll.saturating_add(delta).min(max);
        }
        EventResult::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn detail_mouse_scroll_is_clamped_and_area_bound() {
        let mut panel = DetailPanel::new();
        let target = DetailTarget::Machine("workstation".into());
        panel.scroll_area = Rect::new(2, 3, 12, 8);
        panel.pane_height = 4;
        panel.properties_len = 20;
        panel.properties_scroll = 5;

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollUp, 4, 5), &target),
            EventResult::Consumed
        );
        assert_eq!(panel.properties_scroll, 2);

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 4, 5), &target),
            EventResult::Consumed
        );
        assert_eq!(panel.properties_scroll, 5);

        panel.properties_scroll = u16::MAX;
        panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 4, 5), &target);
        assert_eq!(panel.properties_scroll, 16);

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 1, 5), &target),
            EventResult::Ignored
        );
    }

    #[test]
    fn detail_tab_click_switches_the_active_pane() {
        let mut panel = DetailPanel::new();
        let target = DetailTarget::Machine("workstation".into());
        panel.tab_hitboxes = vec![TitleTabHitbox {
            value: DetailPane::Details,
            area: Rect::new(12, 3, 9, 1),
        }];

        assert_eq!(
            panel.handle_mouse(
                mouse(
                    MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    15,
                    3,
                ),
                &target,
            ),
            EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(
                DetailPane::Details,
            )))
        );
        assert_eq!(panel.active_pane(), DetailPane::Details);
    }

    #[test]
    fn image_targets_expose_only_image_panes() {
        let regular = DetailTarget::Image {
            name: "workstation".into(),
            internal: false,
        };
        assert_eq!(DetailPane::tabs_for(&regular).len(), 3);
        assert!(DetailPane::tabs_for(&regular).contains(&DetailPane::ImageUnit));

        let internal = DetailTarget::Image {
            name: ".oci-layer".into(),
            internal: true,
        };
        assert_eq!(DetailPane::tabs_for(&internal), DetailPane::INTERNAL_IMAGE);
        assert_eq!(DetailPane::tabs_for(&internal).len(), 1);
        assert!(!DetailPane::tabs_for(&internal).contains(&DetailPane::ImageUnit));
    }

    #[test]
    fn machine_and_image_panes_are_remembered_independently() {
        let mut panel = DetailPanel::new();
        let machine = DetailTarget::Machine("workstation".into());
        let image = DetailTarget::Image {
            name: "workstation".into(),
            internal: false,
        };
        let internal_image = DetailTarget::Image {
            name: ".oci-layer".into(),
            internal: true,
        };

        panel.handle_key(
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT),
            &machine,
        );
        panel.ensure_pane_for_target(&image);
        assert_eq!(panel.active_pane, DetailPane::ImageOverview);

        panel.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT), &image);
        panel.ensure_pane_for_target(&machine);
        assert_eq!(panel.active_pane, DetailPane::Metrics);

        panel.ensure_pane_for_target(&internal_image);
        assert_eq!(panel.active_pane, DetailPane::ImageOverview);
        panel.ensure_pane_for_target(&image);
        assert_eq!(panel.active_pane, DetailPane::ImageUnit);
    }
}
