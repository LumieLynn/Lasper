//! Footer content registered for one frame, independent of application state.
//!
//! Focused components register first, followed by their parent's shortcuts.
//! The registry keeps that order, resolves duplicate keys in favor of the
//! first owner and omits less important hints when the footer becomes narrow.

use crate::tui::foundation::{status::StatusLevel, theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Position, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HintPriority {
    Primary,
    Normal,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FooterHint {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    priority: HintPriority,
    shortcut: Option<KeyEvent>,
}

impl FooterHint {
    pub(crate) const fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            priority: HintPriority::Normal,
            shortcut: None,
        }
    }

    pub(crate) const fn primary(mut self) -> Self {
        self.priority = HintPriority::Primary;
        self
    }

    pub(crate) const fn secondary(mut self) -> Self {
        self.priority = HintPriority::Secondary;
        self
    }

    pub(crate) const fn with_label(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    /// A clickable hint uses the same key route as keyboard input.
    pub(crate) const fn on_key(mut self, code: KeyCode) -> Self {
        self.shortcut = Some(KeyEvent::new(code, KeyModifiers::NONE));
        self
    }

    pub(crate) const fn on_key_event(mut self, key: KeyEvent) -> Self {
        self.shortcut = Some(key);
        self
    }

    fn width(self) -> usize {
        self.key.width() + 1 + self.label.width()
    }
}

#[derive(Debug, Default)]
pub(crate) struct FooterRegistry {
    hints: Vec<FooterHint>,
    notice: Option<&'static str>,
}

impl FooterRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register(&mut self, hint: FooterHint) {
        if !self
            .hints
            .iter()
            .any(|registered| registered.key == hint.key)
        {
            self.hints.push(hint);
        }
    }

    pub(crate) fn show_notice(&mut self, notice: &'static str) {
        self.notice = Some(notice);
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) -> FooterHitboxes {
        let (line, hits) = self.layout(area);
        frame.render_widget(Paragraph::new(line), area);
        hits
    }

    #[cfg(test)]
    fn line(&self, width: u16) -> Line<'static> {
        self.layout(Rect::new(0, 0, width, 1)).0
    }

    fn layout(&self, area: Rect) -> (Line<'static>, FooterHitboxes) {
        let mut hits = FooterHitboxes::new(area);
        if let Some(notice) = self.notice {
            return (
                Line::styled(notice, Style::default().fg(theme::theme().hint_fg)),
                hits,
            );
        }
        let mut remaining = usize::from(area.width);
        let mut selected = vec![false; self.hints.len()];
        let mut count = 0;
        for priority in [
            HintPriority::Primary,
            HintPriority::Normal,
            HintPriority::Secondary,
        ] {
            for (index, hint) in self.hints.iter().enumerate() {
                if hint.priority != priority {
                    continue;
                }
                let required = hint.width() + if count > 0 { 2 } else { 0 };
                if required <= remaining {
                    selected[index] = true;
                    remaining -= required;
                    count += 1;
                }
            }
        }

        let t = theme::theme();
        let key_style = Style::default().fg(t.key_hint_fg);
        let label_style = Style::default().fg(t.hint_fg);
        let mut spans = Vec::with_capacity(count * 4);
        let mut x = area.x;
        for (hint, visible) in self.hints.iter().zip(selected) {
            if !visible {
                continue;
            }
            if !spans.is_empty() {
                spans.push(Span::styled(", ", label_style));
                x = x.saturating_add(2);
            }
            hits.items.push(FooterHitbox {
                area: Rect::new(x, area.y, hint.width() as u16, area.height),
                shortcut: hint.shortcut,
            });
            x = x.saturating_add(hint.width() as u16);
            spans.push(Span::styled(hint.key, key_style));
            spans.push(Span::styled(" ", label_style));
            spans.push(Span::styled(hint.label, label_style));
        }

        // If no complete hint fits, retain the most important key. Paragraph
        // clips by terminal cells, including double-width and combining text.
        if spans.is_empty() {
            if let Some(hint) = self.hints.iter().min_by_key(|hint| hint.priority) {
                spans.push(Span::styled(hint.key, key_style));
                hits.items.push(FooterHitbox {
                    area: Rect::new(
                        area.x,
                        area.y,
                        area.width.min(hint.key.width() as u16),
                        area.height,
                    ),
                    shortcut: hint.shortcut,
                });
            }
        }
        (Line::from(spans), hits)
    }
}

#[derive(Default)]
pub(crate) struct FooterHitboxes {
    area: Rect,
    items: Vec<FooterHitbox>,
}

struct FooterHitbox {
    area: Rect,
    shortcut: Option<KeyEvent>,
}

pub(crate) enum FooterPointerInput {
    Consumed,
    Key(KeyEvent),
}

impl FooterHitboxes {
    fn new(area: Rect) -> Self {
        Self {
            area,
            items: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn bounds(&self, code: KeyCode) -> Rect {
        self.items
            .iter()
            .find(|hit| hit.shortcut.is_some_and(|key| key.code == code))
            .map_or(Rect::default(), |hit| hit.area)
    }

    pub(crate) fn handle_mouse(&self, mouse: MouseEvent) -> Option<FooterPointerInput> {
        let position = Position::new(mouse.column, mouse.row);
        if !self.area.contains(position) {
            return None;
        }
        let key = (mouse.kind == MouseEventKind::Down(MouseButton::Left))
            .then(|| {
                self.items
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .and_then(|hit| hit.shortcut)
            })
            .flatten();
        Some(key.map_or(FooterPointerInput::Consumed, FooterPointerInput::Key))
    }
}

pub(crate) enum FooterModel<'a> {
    Hints(FooterRegistry),
    Message {
        text: &'a str,
        level: &'a StatusLevel,
    },
}

impl FooterModel<'_> {
    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) -> FooterHitboxes {
        let line = match self {
            Self::Hints(registry) => return registry.render(frame, area),
            Self::Message { text, level } => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    *text,
                    Style::default().fg(theme::theme().status_color(level)),
                ),
            ]),
        };
        frame.render_widget(Paragraph::new(line), area);
        FooterHitboxes::new(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn text(registry: &FooterRegistry, width: u16) -> String {
        theme::init_theme(theme::Theme::dark());
        registry
            .line(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn first_registration_keeps_ownership_of_a_duplicate_key() {
        let mut registry = FooterRegistry::new();
        registry.register(FooterHint::new("x", "close tab"));
        registry.register(FooterHint::new("x", "actions").primary());

        assert_eq!(text(&registry, 80), "x close tab");
    }

    #[test]
    fn width_selection_prioritizes_hints_but_keeps_registration_order() {
        let mut registry = FooterRegistry::new();
        registry.register(FooterHint::new("q", "quit").secondary());
        registry.register(FooterHint::new("t", "shell"));
        registry.register(FooterHint::new("?", "help").primary());

        assert_eq!(text(&registry, 6), "? help");
        assert_eq!(text(&registry, 15), "t shell, ? help");
        assert_eq!(text(&registry, 23), "q quit, t shell, ? help");
        assert_eq!(text(&registry, 1), "?");
        // A narrow frame does not retain selection or clipping state.
        assert_eq!(text(&registry, 23), "q quit, t shell, ? help");
    }

    #[test]
    fn hint_width_accounts_for_wide_and_combining_characters() {
        let mut registry = FooterRegistry::new();
        registry.register(FooterHint::new("界", "e\u{301}").on_key(KeyCode::Char('x')));
        registry.register(
            FooterHint::new("q", "quit")
                .secondary()
                .on_key(KeyCode::Char('q')),
        );

        assert_eq!(text(&registry, 4), "界 e\u{301}");
        assert_eq!(text(&registry, 11), "界 e\u{301}");
        assert_eq!(text(&registry, 12), "界 e\u{301}, q quit");

        let (_, hits) = registry.layout(Rect::new(5, 7, 12, 1));
        assert_eq!(hits.bounds(KeyCode::Char('x')), Rect::new(5, 7, 4, 1));
        assert_eq!(hits.bounds(KeyCode::Char('q')), Rect::new(11, 7, 6, 1));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            hits.handle_mouse(click),
            Some(FooterPointerInput::Key(key)) if key.code == KeyCode::Char('q')
        ));
        let (_, narrow_hits) = registry.layout(Rect::new(5, 7, 11, 1));
        assert!(matches!(
            narrow_hits.handle_mouse(click),
            Some(FooterPointerInput::Consumed)
        ));
    }

    #[test]
    fn key_only_fallback_is_clipped_by_the_renderer() {
        theme::init_theme(theme::Theme::dark());
        let mut registry = FooterRegistry::new();
        registry.register(FooterHint::new("界a", "action").primary());
        let mut terminal = Terminal::new(TestBackend::new(2, 1)).unwrap();
        terminal
            .draw(|frame| {
                FooterModel::Hints(registry).render(frame, frame.area());
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
    }
}
