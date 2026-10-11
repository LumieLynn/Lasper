use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::tui::foundation::scrolling::{ScrollAction, ScrollState};
use crate::tui::protocol::EventResult;

/// Shared vertical scrollbar styling for scrollable TUI panels.
fn vertical_scrollbar() -> Scrollbar<'static> {
    Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("▐")
        .track_symbol(Some("│"))
}

/// The owner supplies its scroll state; the scrollbar keeps only the geometry
/// from the last frame. Arrows scroll one row and the track pages toward the
/// click, without changing the focused control or interpreting its input.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VerticalScrollbar {
    area: Rect,
    thumb_start: u16,
    thumb_end: u16,
}

impl VerticalScrollbar {
    pub(crate) fn clear_mouse_area(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, scroll: &ScrollState) {
        self.clear_mouse_area();
        if scroll.max_offset() == 0 || area.is_empty() {
            return;
        }
        let area = Rect::new(area.right() - 1, area.y, 1, area.height).intersection(frame.area());
        // Ratatui requires a track between the two arrow heads.
        if area.height < 3 || area.is_empty() {
            return;
        }
        let mut state = ScrollbarState::new(scroll.max_offset().saturating_add(1))
            .position(scroll.offset())
            .viewport_content_length(scroll.viewport_rows());
        frame.render_stateful_widget(vertical_scrollbar(), area, &mut state);

        let track = area.height - 2;
        let viewport = if scroll.viewport_rows() == 0 {
            usize::from(area.height)
        } else {
            scroll.viewport_rows()
        };
        let total = scroll.max_offset().saturating_add(viewport);
        // Match Ratatui's rounded thumb lengths, including its minimum of one
        // cell. Wide arithmetic keeps large documents safe from multiplication
        // overflow while all coordinates still fit within the rendered area.
        let scaled = |rows: usize| {
            ((rows as u128 * u128::from(track) + total as u128 / 2) / total as u128) as u16
        };
        let thumb_length = scaled(viewport).clamp(1, track);
        let thumb_start = scaled(scroll.offset()).min(track - thumb_length);
        self.area = area;
        self.thumb_start = area.y + 1 + thumb_start;
        self.thumb_end = self.thumb_start + thumb_length;
    }

    pub(crate) fn handle_mouse(&self, mouse: MouseEvent, scroll: &mut ScrollState) -> EventResult {
        if scroll.max_offset() == 0 || !self.area.contains((mouse.column, mouse.row).into()) {
            return EventResult::Ignored;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return scroll.handle_mouse(mouse.kind);
        }
        let action = if mouse.row == self.area.y {
            Some(ScrollAction::Up(1))
        } else if mouse.row == self.area.bottom() - 1 {
            Some(ScrollAction::Down(1))
        } else if mouse.row < self.thumb_start {
            Some(ScrollAction::PageUp)
        } else if mouse.row >= self.thumb_end {
            Some(ScrollAction::PageDown)
        } else {
            None
        };
        if let Some(action) = action {
            scroll.apply(action);
        }
        EventResult::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render(
        bar: &mut VerticalScrollbar,
        scroll: &ScrollState,
        area: Rect,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(30, 30)).unwrap();
        terminal
            .draw(|frame| bar.render(frame, area, scroll))
            .unwrap();
        terminal
    }

    #[test]
    fn arrows_and_track_clicks_use_the_existing_scroll_bounds() {
        let mut bar = VerticalScrollbar::default();
        let area = Rect::new(4, 3, 1, 12);
        let mut scroll = ScrollState::default();
        scroll.update(100, 10);
        let click = |row| mouse(MouseEventKind::Down(MouseButton::Left), area.x, row);
        render(&mut bar, &scroll, area);
        assert_eq!(
            bar.handle_mouse(click(area.y), &mut scroll),
            EventResult::Consumed
        );
        assert_eq!(scroll.offset(), 0);
        bar.handle_mouse(click(area.bottom() - 1), &mut scroll);
        assert_eq!(scroll.offset(), 1);
        render(&mut bar, &scroll, area);
        bar.handle_mouse(click(area.bottom() - 2), &mut scroll);
        assert_eq!(scroll.offset(), 10);

        scroll.apply(ScrollAction::Last);
        render(&mut bar, &scroll, area);
        bar.handle_mouse(click(area.bottom() - 1), &mut scroll);
        assert_eq!(scroll.offset(), 90);
        bar.handle_mouse(click(area.y + 1), &mut scroll);
        assert_eq!(scroll.offset(), 81);

        render(&mut bar, &scroll, area);
        bar.handle_mouse(click(bar.thumb_start), &mut scroll);
        assert_eq!(scroll.offset(), 81);
        assert_eq!(
            bar.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Right), area.x, area.y),
                &mut scroll
            ),
            EventResult::Ignored
        );
        assert_eq!(
            bar.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), area.x - 1, area.y),
                &mut scroll
            ),
            EventResult::Ignored
        );
    }

    #[test]
    fn hit_geometry_matches_the_thumb_and_is_released_when_hidden() {
        let area = Rect::new(4, 3, 1, 20);
        let mut bar = VerticalScrollbar::default();
        let mut scroll = ScrollState::default();
        for (content, viewport, position) in [
            (100, 10, 0),
            (100, 10, 50),
            (100, 10, 90),
            (21, 20, 1),
            (usize::MAX, 20, 0),
        ] {
            scroll.update(content, viewport);
            scroll.reset();
            scroll.apply(ScrollAction::Down(position));
            let terminal = render(&mut bar, &scroll, area);
            for row in area.y + 1..area.bottom() - 1 {
                assert_eq!(
                    terminal.backend().buffer()[(area.x, row)].symbol() == "▐",
                    (bar.thumb_start..bar.thumb_end).contains(&row)
                );
            }
        }
        scroll.update(10, 20);
        render(&mut bar, &scroll, area);
        scroll.update(100, 10);
        let click = mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.bottom() - 1,
        );
        assert_eq!(bar.handle_mouse(click, &mut scroll), EventResult::Ignored);
        for area in [Rect::ZERO, Rect::new(4, 3, 1, 1), Rect::new(4, 3, 1, 2)] {
            render(&mut bar, &scroll, area);
            assert_eq!(bar.handle_mouse(click, &mut scroll), EventResult::Ignored);
        }
    }
}
