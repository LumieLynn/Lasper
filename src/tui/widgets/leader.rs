use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::tui::core::{Component, EventResult};
use crate::tui::theme;

/// The one-level keymap shown after the workspace leader key is pressed.
///
/// This is a component rather than a boolean render flag so the transient
/// layer has the same focus lifecycle as the other modal UI elements.
#[derive(Debug, Default)]
pub struct LeaderOverlay {
    focused: bool,
}

impl LeaderOverlay {
    pub fn new() -> Self {
        Self { focused: true }
    }
}

impl Component for LeaderOverlay {
    fn render(&mut self, f: &mut Frame, content_area: Rect) {
        let width = 32u16.min(content_area.width.saturating_sub(2));
        let height = 4u16.min(content_area.height.saturating_sub(2));
        if width < 10 || height < 3 {
            return;
        }

        let area = Rect::new(
            content_area
                .x
                .saturating_add(content_area.width.saturating_sub(width + 1)),
            content_area
                .y
                .saturating_add(content_area.height.saturating_sub(height + 1)),
            width,
            height,
        );
        let t = theme::theme();
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Leader ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.accent));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![key_span(" t "), hint_span(" selected-user shell")]),
                Line::from(vec![key_span(" l "), hint_span(" login terminal")]),
            ]),
            inner,
        );
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Consumed
    }

    fn handle_mouse(&mut self, _mouse: MouseEvent) -> EventResult {
        EventResult::Consumed
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn is_focusable(&self) -> bool {
        true
    }
}

fn key_span(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(theme::theme().key_hint_fg))
}

fn hint_span(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(theme::theme().hint_fg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_focusable_and_tracks_focus() {
        let mut overlay = LeaderOverlay::new();
        assert!(overlay.is_focusable());
        assert!(overlay.is_focused());

        overlay.set_focus(false);
        assert!(!overlay.is_focused());
    }
}
