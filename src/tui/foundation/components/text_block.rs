//! A borrowed text provider composed with scrolling, a border and input hints.
use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::scrolling::{ScrollAction, ScrollState, SCROLL_HINTS};
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crate::tui::foundation::widgets::display::scrollbar::VerticalScrollbar;
use crate::tui::foundation::widgets::display::text::TextContent;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders},
    Frame,
};

#[derive(Default)]
pub(crate) struct TextBlockState {
    pub(crate) scroll: ScrollState,
    scrollbar: VerticalScrollbar,
    area: Rect,
}

impl TextBlockState {
    pub(crate) fn clear_mouse_area(&mut self) {
        self.area = Rect::default();
        self.scrollbar.clear_mouse_area();
    }

    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        content: &mut impl TextContent,
        follow_tail: bool,
    ) {
        self.clear_mouse_area();
        if area.is_empty() {
            return;
        }
        let following = follow_tail && self.scroll.offset() == self.scroll.max_offset();
        let layout = content.layout(area.width, self.scroll.offset());
        self.scroll.update(layout.height, usize::from(area.height));
        self.scroll.reset();
        self.scroll.apply(ScrollAction::Down(layout.top_row));
        if following {
            self.scroll.apply(ScrollAction::Last);
        }
        content.render(frame, area, self.scroll.offset());
        self.scrollbar.render(
            frame,
            Rect::new(area.right(), area.y, 1, area.height),
            &self.scroll,
        );
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> ControlOutcome {
        if self.scrollbar.handle_mouse(mouse, &mut self.scroll)
            == crate::tui::protocol::EventResult::Consumed
        {
            return ControlOutcome::Consumed;
        }
        if self.area.contains((mouse.column, mouse.row).into()) {
            if let Some(action) = ScrollAction::from_mouse(mouse.kind) {
                self.scroll.apply(action);
                return ControlOutcome::Consumed;
            }
        }
        ControlOutcome::Ignored
    }
}

pub(crate) struct TextBlockControl<'a, C> {
    state: &'a mut TextBlockState,
    block: Block<'a>,
    content: C,
    follow_tail: bool,
}

impl<'a, C: TextContent> TextBlockControl<'a, C> {
    pub(crate) fn new(state: &'a mut TextBlockState, label: &'a str, content: C) -> Self {
        Self {
            state,
            block: Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(label),
            content,
            follow_tail: false,
        }
    }

    pub(crate) fn with_block(mut self, block: Block<'a>) -> Self {
        self.block = block;
        self
    }

    /// The containing view chooses this behavior; text layout does not scroll.
    pub(crate) fn follow_tail(mut self) -> Self {
        self.follow_tail = true;
        self
    }

    pub(crate) fn preferred_height(&mut self, width: u16) -> u16 {
        let bounds = Rect::new(0, 0, width, u16::MAX);
        let inner = self.block.inner(bounds);
        let rows = self.content.layout(inner.width, 0).height;
        (rows.min(usize::from(u16::MAX)) as u16).saturating_add(bounds.height - inner.height)
    }
}

impl<C: TextContent> Control for TextBlockControl<'_, C> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        let block = self.block.clone().border_style(Style::default().fg(
            crate::tui::foundation::theme::widget_border_color(context.focused, context.enabled),
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.state
            .render(frame, inner, &mut self.content, self.follow_tail);
        if !inner.is_empty() {
            self.state.area = area;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        if let Some(action) = ScrollAction::from_key(key) {
            self.state.scroll.apply(action);
            ControlOutcome::Consumed
        } else {
            ControlOutcome::Ignored
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _area: Rect) -> ControlOutcome {
        self.state.handle_mouse(mouse)
    }

    fn hints(&self) -> &'static [FooterHint] {
        SCROLL_HINTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::foundation::widgets::display::text::PlainText;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn preferred_height_fits_the_rendered_text_at_different_widths() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let content = "中文与 emoji 🪐\nwords that wrap at narrow widths    \nEND";
        for width in [8, 20, 40] {
            let mut state = TextBlockState::default();
            let mut content = PlainText::new(content);
            let mut control = TextBlockControl::new(&mut state, "Text", &mut content);
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
