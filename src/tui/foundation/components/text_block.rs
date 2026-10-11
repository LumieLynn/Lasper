//! Scrollable text borrows its content; the containing form owns focus and hits.
use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::scrolling::{ScrollAction, ScrollState};
use crate::tui::foundation::text::soft_wrap_text;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crate::tui::foundation::widgets::display::scrollbar::VerticalScrollbar;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

const TEXT_HINTS: &[FooterHint] = &[
    FooterHint::new("↑/↓ / j/k", "scroll").secondary(),
    FooterHint::new("PgUp/PgDn", "page").secondary(),
    FooterHint::new("Home/End", "top/bottom").secondary(),
];
#[derive(Default)]
pub(crate) struct TextBlockState {
    scroll: ScrollState,
    scrollbar: VerticalScrollbar,
}

pub(crate) struct TextBlockControl<'a> {
    state: &'a mut TextBlockState,
    label: &'a str,
    content: &'a str,
}
impl<'a> TextBlockControl<'a> {
    pub(crate) fn new(state: &'a mut TextBlockState, label: &'a str, content: &'a str) -> Self {
        Self {
            state,
            label,
            content,
        }
    }

    fn block(&self) -> Block<'a> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(self.label)
    }

    /// Include the control's borders and use the same wrapping as drawing.
    pub(crate) fn preferred_height(&self, width: u16) -> u16 {
        let bounds = Rect::new(0, 0, width, u16::MAX);
        let inner = self.block().inner(bounds);
        let rows = soft_wrap_text(self.content, usize::from(inner.width.max(1))).len();
        (rows.min(usize::from(u16::MAX)) as u16).saturating_add(bounds.height - inner.height)
    }
}
impl Control for TextBlockControl<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        let block = self.block().border_style(Style::default().fg(
            crate::tui::foundation::theme::widget_border_color(context.focused, context.enabled),
        ));
        let inner = block.inner(area);
        let lines = soft_wrap_text(self.content, usize::from(inner.width.max(1)));
        self.state
            .scroll
            .update(lines.len(), usize::from(inner.height));
        let visible: Vec<_> = lines
            .into_iter()
            .skip(self.state.scroll.offset())
            .take(usize::from(inner.height))
            .map(ratatui::text::Line::from)
            .collect();
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(visible), inner);
        self.state.scrollbar.render(
            frame,
            Rect::new(inner.right(), inner.y, 1, inner.height),
            &self.state.scroll,
        );
    }
    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        if let Some(action) = ScrollAction::from_key(key) {
            self.state.scroll.apply(action);
            ControlOutcome::Consumed
        } else {
            ControlOutcome::Ignored
        }
    }
    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> ControlOutcome {
        if self
            .state
            .scrollbar
            .handle_mouse(mouse, &mut self.state.scroll)
            == crate::tui::protocol::EventResult::Consumed
        {
            return ControlOutcome::Consumed;
        }
        if area.contains((mouse.column, mouse.row).into()) {
            if let Some(action) = ScrollAction::from_mouse(mouse.kind) {
                self.state.scroll.apply(action);
                return ControlOutcome::Consumed;
            }
        }
        ControlOutcome::Ignored
    }
    fn hints(&self) -> &'static [FooterHint] {
        TEXT_HINTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn preferred_height_fits_the_rendered_text_at_different_widths() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let content = "中文与 emoji 🪐\nwords that wrap at narrow widths    \nEND";
        for width in [8, 20, 40] {
            let mut state = TextBlockState::default();
            let mut control = TextBlockControl::new(&mut state, "Text", content);
            let height = control.preferred_height(width);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    control.render(
                        frame,
                        frame.area(),
                        ControlContext {
                            focused: true,
                            enabled: true,
                        },
                    );
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("END"));
            assert_eq!(state.scroll.max_offset(), 0);
        }
    }
}
