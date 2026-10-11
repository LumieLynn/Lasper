use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

use crate::tui::features::workspace::inspector::core::document_cache::DocumentCache;
use crate::tui::features::workspace::inspector::core::model::{
    DetailPane, DetailPaneContext, DetailTarget,
};
use crate::tui::features::workspace::inspector::core::scrolling;
use crate::tui::features::workspace::inspector::panes;
use crate::tui::features::workspace::inspector::presentation::viewport;
use crate::tui::features::workspace::inspector::DetailRenderContext;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::scrolling::{register_scroll_hints, ScrollAction, ScrollState};
use crate::tui::foundation::widgets::display::scrollbar::VerticalScrollbar;
use crate::tui::foundation::widgets::display::title_tabs::{
    clicked_title_tab, TitleTabHitbox, TitleTabViewport,
};
use crate::tui::protocol::{AppMessage, ContainerMessage, EventResult};

pub struct DetailPanel {
    active_pane: DetailPane,
    remembered_machine_pane: DetailPane,
    remembered_image_pane: DetailPane,
    pane_context: DetailPaneContext,
    pub(crate) details_scroll: ScrollState,
    pub(crate) machine_overview_scroll: ScrollState,
    pub(crate) log_scroll: ScrollState,
    pub(crate) config_scroll: ScrollState,
    pub pane_height: u16,
    pub focused: bool,
    pub(crate) details_len: usize,
    pub(crate) machine_overview_len: usize,
    pub(crate) image_overview_len: usize,
    pub(crate) logs_len: usize,
    pub(crate) config_len: usize,
    pub(crate) last_rendered_width: u16,
    pub(crate) log_cache: panes::logs::LogRenderCache,
    pub(crate) log_target: Option<String>,
    pub(crate) document_cache: DocumentCache,
    scroll_area: Rect,
    scrollbar: VerticalScrollbar,
    tab_hitboxes: Vec<TitleTabHitbox<DetailPane>>,
    tab_viewport: TitleTabViewport,
}

impl DetailPanel {
    pub fn new() -> Self {
        Self {
            active_pane: DetailPane::MachineOverview,
            remembered_machine_pane: DetailPane::MachineOverview,
            remembered_image_pane: DetailPane::ImageOverview,
            pane_context: DetailPaneContext::Machine,
            details_scroll: ScrollState::default(),
            machine_overview_scroll: ScrollState::default(),
            log_scroll: ScrollState::default(),
            config_scroll: ScrollState::default(),
            pane_height: 10,
            focused: false,
            details_len: 0,
            machine_overview_len: 0,
            image_overview_len: 0,
            logs_len: 0,
            config_len: 0,
            last_rendered_width: 0,
            log_cache: panes::logs::LogRenderCache::default(),
            log_target: None,
            document_cache: DocumentCache::default(),
            scroll_area: Rect::default(),
            scrollbar: VerticalScrollbar::default(),
            tab_hitboxes: Vec::new(),
            tab_viewport: TitleTabViewport::default(),
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn active_pane(&self) -> DetailPane {
        self.active_pane
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry, target: &DetailTarget) {
        let pane_hint = match DetailPane::tabs_for(target).len() {
            1 => "Alt+1",
            2 => "Alt+1..2",
            3 => "Alt+1..3",
            4 => "Alt+1..4",
            _ => "Alt+1..5",
        };
        footer.register(FooterHint::new(pane_hint, "panes").primary());
        if DetailPane::tabs_for(target).len() > 1 {
            footer.register(FooterHint::new("[/]", "cycle"));
        }
        if self.active_pane != DetailPane::Metrics {
            register_scroll_hints(footer);
        }
    }

    pub(crate) fn render(
        &mut self,
        f: &mut Frame,
        area: Rect,
        data: &mut DetailRenderContext<'_>,
        resize_mode: bool,
    ) {
        // Border
        let border_color =
            crate::tui::foundation::theme::panel_border_color(resize_mode, self.focused, false);

        self.ensure_pane_for_target(data.target);
        let labels = Self::tab_labels(data);
        let tabs = DetailPane::tabs_for(data.target);
        debug_assert_eq!(tabs.len(), labels.len());
        let active = tabs
            .iter()
            .position(|pane| pane == &self.active_pane)
            .unwrap_or(0);
        let t = crate::tui::foundation::theme::theme();
        let tab_specs = tabs
            .iter()
            .copied()
            .zip(labels)
            .enumerate()
            .map(|(index, (pane, label))| {
                let style = if index == active {
                    Style::default()
                        .fg(if self.focused {
                            t.tab_active_focused
                        } else {
                            t.tab_active_unfocused
                        })
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.tab_inactive)
                };
                (pane, label.to_owned(), style)
            })
            .collect::<Vec<_>>();
        let tab_layout = self.tab_viewport.layout(
            area,
            &tab_specs,
            "-",
            active,
            Style::default().fg(t.tab_inactive),
        );
        self.tab_hitboxes = tab_layout.hitboxes;
        let tabs_line = tab_layout.line;

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(tabs_line);

        // Get inner area
        let inner_area = block.inner(area);
        self.scroll_area = inner_area;
        self.pane_height = inner_area.height;

        // The scrollbar sits on the border. Count the same width the panes render.
        let pane_width = usize::from(inner_area.width);

        scrolling::sync_viewport(self, data, pane_width);

        f.render_widget(Clear, area);
        f.render_widget(block, area);

        // Render content area directly in inner_area
        match self.active_pane {
            DetailPane::Logs => panes::logs::render(
                f,
                data,
                inner_area,
                self.log_scroll.offset(),
                &self.log_cache,
            ),
            DetailPane::Metrics => panes::metrics::render(f, data, inner_area),
            _ => {
                let scroll = self
                    .active_scroll_mut()
                    .map(|scroll| scroll.offset())
                    .unwrap_or(0);
                viewport::render(f, self.document_cache.lines(), inner_area, scroll);
            }
        }

        self.scrollbar.clear_mouse_area();
        if let Some(scroll) = self.active_scroll_mut().copied() {
            self.scrollbar.render(
                f,
                Rect::new(
                    area.right().saturating_sub(1),
                    inner_area.y,
                    1,
                    inner_area.height,
                ),
                &scroll,
            );
        }
    }

    fn tab_labels(data: &DetailRenderContext<'_>) -> Vec<&'static str> {
        if data.target.is_internal_image() {
            vec![" Overview "]
        } else if data.target.is_image() {
            vec![" Overview ", " Config ", " Details "]
        } else {
            let stopped = data
                .machine
                .is_none_or(|entry| !entry.state.accepts_runtime_actions());
            vec![
                " Overview ",
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

    /// Keyboard, mouse and scrollbar all use the same active viewport.
    fn active_scroll_mut(&mut self) -> Option<&mut ScrollState> {
        let (scroll, length) = match self.active_pane {
            DetailPane::MachineOverview => {
                (&mut self.machine_overview_scroll, self.machine_overview_len)
            }
            DetailPane::Details => (&mut self.details_scroll, self.details_len),
            DetailPane::ImageOverview => (&mut self.details_scroll, self.image_overview_len),
            DetailPane::Config | DetailPane::ImageConfig => {
                (&mut self.config_scroll, self.config_len)
            }
            DetailPane::Logs => (&mut self.log_scroll, self.logs_len),
            DetailPane::Metrics => return None,
        };
        scroll.update(length, usize::from(self.pane_height));
        Some(scroll)
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
            DetailPane::MachineOverview => self.machine_overview_scroll.reset(),
            DetailPane::Details => self.details_scroll.reset(),
            DetailPane::Logs => {
                self.log_scroll
                    .update(self.logs_len, usize::from(self.pane_height));
                self.log_scroll.apply(ScrollAction::Last);
            }
            DetailPane::Config | DetailPane::ImageConfig => self.config_scroll.reset(),
            DetailPane::Metrics => {}
            DetailPane::ImageOverview => self.details_scroll.reset(),
        }
        EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(pane)))
    }

    /// Handles all keyboard input for the detail panel.
    /// Returns Consumed for scroll/navigation, Message for pane switches.
    pub fn handle_key(&mut self, key: KeyEvent, target: &DetailTarget) -> EventResult {
        self.ensure_pane_for_target(target);

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

            _ => {}
        }

        match self.active_scroll_mut() {
            Some(scroll) => scroll.handle_key(key),
            None => EventResult::Ignored,
        }
    }

    /// Switch tabs or scroll the active detail pane.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, target: &DetailTarget) -> EventResult {
        self.ensure_pane_for_target(target);
        if let Some(pane) = clicked_title_tab(&self.tab_hitboxes, mouse) {
            if DetailPane::tabs_for(target).contains(&pane) {
                return self.switch_pane(pane, target);
            }
        }

        let scrollbar = self.scrollbar;
        if let Some(scroll) = self.active_scroll_mut() {
            if scrollbar.handle_mouse(mouse, scroll) == EventResult::Consumed {
                return EventResult::Consumed;
            }
        }

        if mouse.column < self.scroll_area.x
            || mouse.column >= self.scroll_area.x.saturating_add(self.scroll_area.width)
            || mouse.row < self.scroll_area.y
            || mouse.row >= self.scroll_area.y.saturating_add(self.scroll_area.height)
        {
            return EventResult::Ignored;
        }

        if !matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return EventResult::Ignored;
        }

        match self.active_scroll_mut() {
            Some(scroll) => scroll.handle_mouse(mouse.kind),
            None => EventResult::Consumed,
        }
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
        panel.machine_overview_len = 20;
        panel.machine_overview_scroll.update(20, 4);
        panel.machine_overview_scroll.apply(ScrollAction::Down(5));

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollUp, 4, 5), &target),
            EventResult::Consumed
        );
        assert_eq!(panel.machine_overview_scroll.offset(), 2);

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 4, 5), &target),
            EventResult::Consumed
        );
        assert_eq!(panel.machine_overview_scroll.offset(), 5);

        panel.machine_overview_scroll.apply(ScrollAction::Last);
        panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 4, 5), &target);
        assert_eq!(panel.machine_overview_scroll.offset(), 16);

        assert_eq!(
            panel.handle_mouse(mouse(MouseEventKind::ScrollDown, 1, 5), &target),
            EventResult::Ignored
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 20)).unwrap();
        terminal
            .draw(|frame| {
                panel.scrollbar.render(
                    frame,
                    Rect::new(14, 3, 1, 8),
                    &panel.machine_overview_scroll,
                )
            })
            .unwrap();
        // The scrollbar lies on the border, outside the content viewport.
        assert_eq!(
            panel.handle_mouse(
                mouse(
                    MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    14,
                    3
                ),
                &target
            ),
            EventResult::Consumed
        );
        assert_eq!(panel.machine_overview_scroll.offset(), 15);
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
        assert!(DetailPane::tabs_for(&regular).contains(&DetailPane::Details));

        let internal = DetailTarget::Image {
            name: ".oci-layer".into(),
            internal: true,
        };
        assert_eq!(DetailPane::tabs_for(&internal), DetailPane::INTERNAL_IMAGE);
        assert_eq!(DetailPane::tabs_for(&internal).len(), 1);
        assert!(!DetailPane::tabs_for(&internal).contains(&DetailPane::Details));
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
        assert_eq!(panel.active_pane, DetailPane::Details);
    }
}
