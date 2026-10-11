//! Scroll input and viewport bounds shared by text panes and form viewports.
//! Lists reuse the actions for selection; terminal input decides when to pass
//! keys through before using these actions to browse scrollback.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseEventKind};

use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::protocol::EventResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollAction {
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    First,
    Last,
}

impl ScrollAction {
    pub(crate) fn from_key(key: KeyEvent) -> Option<Self> {
        if !key.modifiers.is_empty() || key.kind == KeyEventKind::Release {
            return None;
        }
        Some(match key.code {
            KeyCode::Up | KeyCode::Char('k') => Self::Up(1),
            KeyCode::Down | KeyCode::Char('j') => Self::Down(1),
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Home => Self::First,
            KeyCode::End => Self::Last,
            _ => return None,
        })
    }

    pub(crate) fn from_mouse(kind: MouseEventKind) -> Option<Self> {
        match kind {
            MouseEventKind::ScrollUp => Some(Self::Up(3)),
            MouseEventKind::ScrollDown => Some(Self::Down(3)),
            _ => None,
        }
    }

    /// Terminal scrollback counts from the bottom instead of the top.
    pub(crate) fn reversed(self) -> Self {
        match self {
            Self::Up(rows) => Self::Down(rows),
            Self::Down(rows) => Self::Up(rows),
            Self::PageUp => Self::PageDown,
            Self::PageDown => Self::PageUp,
            Self::First => Self::Last,
            Self::Last => Self::First,
        }
    }

    /// Also used for list selection, where the last item is the upper bound.
    pub(crate) fn destination(self, offset: usize, max: usize, viewport_rows: usize) -> usize {
        let offset = offset.min(max);
        let page = viewport_rows.saturating_sub(1).max(1);
        match self {
            Self::Up(rows) => offset.saturating_sub(rows),
            Self::Down(rows) => offset.saturating_add(rows).min(max),
            Self::PageUp => offset.saturating_sub(page),
            Self::PageDown => offset.saturating_add(page).min(max),
            Self::First => 0,
            Self::Last => max,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScrollState {
    offset: usize,
    content_rows: usize,
    viewport_rows: usize,
}

impl ScrollState {
    pub(crate) fn update(&mut self, content_rows: usize, viewport_rows: usize) {
        self.content_rows = content_rows;
        self.viewport_rows = viewport_rows;
        self.offset = self.offset.min(self.max_offset());
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) fn max_offset(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    pub(crate) fn viewport_rows(&self) -> usize {
        self.viewport_rows
    }

    pub(crate) fn reset(&mut self) {
        self.offset = 0;
    }

    pub(crate) fn apply(&mut self, action: ScrollAction) {
        self.offset = action.destination(self.offset, self.max_offset(), self.viewport_rows);
    }

    /// Reveal the focused control after focus changes, rather than undoing
    /// pointer scrolling on every render. An oversized control starts at its
    /// first row so its label remains visible.
    pub(crate) fn reveal(&mut self, rows: Range<usize>) {
        if rows.start < self.offset || rows.len() > self.viewport_rows {
            self.offset = rows.start;
        } else if rows.end > self.offset.saturating_add(self.viewport_rows) {
            self.offset = rows.end.saturating_sub(self.viewport_rows);
        }
        self.offset = self.offset.min(self.max_offset());
    }

    pub(crate) fn visible_rows(&self, rows: Range<usize>) -> Option<Range<usize>> {
        let start = rows.start.max(self.offset);
        let end = rows.end.min(self.offset.saturating_add(self.viewport_rows));
        (start < end).then_some(start..end)
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match ScrollAction::from_key(key) {
            Some(action) => {
                self.apply(action);
                EventResult::Consumed
            }
            None => EventResult::Ignored,
        }
    }

    pub(crate) fn handle_mouse(&mut self, kind: MouseEventKind) -> EventResult {
        match ScrollAction::from_mouse(kind) {
            Some(action) => {
                self.apply(action);
                EventResult::Consumed
            }
            None => EventResult::Ignored,
        }
    }
}

pub(crate) fn register_scroll_hints(footer: &mut FooterRegistry) {
    footer.register(FooterHint::new("↑/↓ / j/k", "scroll"));
    footer.register(FooterHint::new("PgUp/PgDn", "page").secondary());
    footer.register(FooterHint::new("Home/End", "top/bottom").secondary());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn viewport_changes_clamp_offsets_and_page_keys_overlap_one_row() {
        let mut scroll = ScrollState::default();
        scroll.update(100, 20);
        scroll.apply(ScrollAction::PageDown);
        assert_eq!(scroll.offset(), 19);
        scroll.apply(ScrollAction::Last);
        assert_eq!(scroll.offset(), 80);
        scroll.update(100, 90);
        assert_eq!(scroll.offset(), 10);
        scroll.update(10, 90);
        assert_eq!(scroll.offset(), 0);
        scroll.update(usize::MAX, 0);
        scroll.apply(ScrollAction::Last);
        scroll.apply(ScrollAction::Down(3));
        assert_eq!(scroll.offset(), usize::MAX);
    }

    #[test]
    fn focus_reveal_and_bottom_anchored_offsets_use_the_same_bounds() {
        let mut scroll = ScrollState::default();
        scroll.update(100, 20);
        scroll.reveal(30..33);
        assert_eq!(scroll.offset(), 13);
        scroll.reveal(5..8);
        assert_eq!(scroll.offset(), 5);
        scroll.reveal(30..80);
        assert_eq!(scroll.offset(), 30);
        assert_eq!(ScrollAction::PageUp.reversed().destination(0, 80, 20), 19);
        assert_eq!(ScrollAction::First.reversed().destination(10, 80, 20), 80);
        assert_eq!(ScrollAction::Last.reversed().destination(10, 80, 20), 0);
    }

    #[test]
    fn modified_keys_remain_available_to_controls_and_terminal_programs() {
        assert_eq!(
            ScrollAction::from_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Some(ScrollAction::First)
        );
        assert_eq!(
            ScrollAction::from_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
    }
}
