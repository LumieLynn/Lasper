use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Frame,
};

use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::selection::{navigate_selection, register_selection_hints};
use crate::tui::foundation::theme;
use crate::tui::protocol::{AppMessage, EventResult, ListMessage};

#[derive(Clone, Copy)]
pub struct RowStyles {
    pub text: Style,
    cursor: Style,
    selected: bool,
}

impl RowStyles {
    pub fn cursor_span(self) -> Span<'static> {
        Span::styled(if self.selected { ">> " } else { "   " }, self.cursor)
    }
}

pub struct ResourceList {
    state: ListState,
    label: String,
    focused: bool,
    body_area: Rect,
    rendered_len: usize,
}

pub struct ResourceListRender<'a> {
    pub selected: usize,
    pub resize_mode: bool,
    pub empty_message: &'a str,
    pub trailing_title: Option<Line<'a>>,
}

impl ResourceList {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            state: ListState::default(),
            label: label.into(),
            focused: false,
            body_area: Rect::default(),
            rendered_len: 0,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_selection_hints(footer);
    }

    pub fn handle_key(&self, key: KeyEvent, len: usize, selected: usize) -> EventResult {
        if len == 0 {
            return EventResult::Ignored;
        }
        match ScrollAction::from_key(key) {
            Some(ScrollAction::Down(1)) => {
                EventResult::Message(AppMessage::List(ListMessage::Next))
            }
            Some(ScrollAction::Up(1)) => EventResult::Message(AppMessage::List(ListMessage::Prev)),
            Some(action) => navigate_selection(
                action,
                Some(selected.min(len - 1)),
                len,
                usize::from(self.body_area.height),
                false,
                |_| true,
            )
            .map_or(EventResult::Consumed, |index| {
                EventResult::Message(AppMessage::List(ListMessage::Select(index)))
            }),
            None => EventResult::Ignored,
        }
    }

    /// Rows and wheel navigation use the same selection messages as keys.
    /// The rendered offset matters when the list is longer than its panel.
    pub fn handle_mouse(&self, mouse: MouseEvent) -> EventResult {
        if self.rendered_len == 0 || !self.body_area.contains((mouse.column, mouse.row).into()) {
            return EventResult::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let index = self.state.offset() + usize::from(mouse.row - self.body_area.y);
                if index < self.rendered_len {
                    EventResult::Message(AppMessage::List(ListMessage::Select(index)))
                } else {
                    EventResult::Consumed
                }
            }
            MouseEventKind::ScrollDown => EventResult::Message(AppMessage::List(ListMessage::Next)),
            MouseEventKind::ScrollUp => EventResult::Message(AppMessage::List(ListMessage::Prev)),
            _ => EventResult::Ignored,
        }
    }

    pub fn render<'a, T, F>(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        entries: &'a [T],
        options: ResourceListRender<'a>,
        mut render_row: F,
    ) where
        F: FnMut(&'a T, RowStyles) -> ListItem<'a>,
    {
        self.sync_selection(entries.len(), options.selected);
        let block = self.block(options.resize_mode, options.trailing_title);
        self.body_area = block.inner(area);
        self.rendered_len = entries.len();

        if entries.is_empty() {
            let paragraph = Paragraph::new(options.empty_message)
                .style(Style::default().fg(theme::theme().list_empty))
                .block(block.padding(Padding::horizontal(1)))
                .wrap(Wrap { trim: true });
            frame.render_widget(paragraph, area);
            return;
        }

        let selected_index = self.state.selected();
        let items = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| render_row(entry, self.row_styles(Some(index) == selected_index)))
            .collect::<Vec<_>>();

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default());
        frame.render_stateful_widget(list, area, &mut self.state);
    }

    fn sync_selection(&mut self, len: usize, selected: usize) {
        self.state.select(if len == 0 {
            None
        } else {
            Some(selected.min(len - 1))
        });
    }

    fn row_styles(&self, selected: bool) -> RowStyles {
        let t = theme::theme();
        let text = if selected {
            let style = Style::default().fg(if self.focused {
                t.list_selected_focused
            } else {
                t.list_selected_unfocused
            });
            if self.focused {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        } else {
            Style::default().fg(t.list_unselected)
        };
        let cursor = if self.focused {
            Style::default()
                .fg(t.list_cursor_focused)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.list_cursor_unfocused)
        };
        RowStyles {
            text,
            cursor,
            selected,
        }
    }

    fn block<'a>(&self, resize_mode: bool, trailing_title: Option<Line<'a>>) -> Block<'a> {
        let mut block = Block::default()
            .title(self.label.clone())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default().fg(crate::tui::foundation::theme::panel_border_color(
                    resize_mode,
                    self.focused,
                    true,
                )),
            );
        if let Some(title) = trailing_title {
            block = block.title(title.right_aligned());
        }
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn empty_resource_messages_wrap_inside_the_panel() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());

        for (message, expected_rows) in [
            (
                "No running machines found",
                ["No running", "machines found"],
            ),
            ("No regular images found", ["No regular", "images found"]),
        ] {
            let rows = render_empty(message);
            for (row, expected) in rows[1..=2].iter().zip(expected_rows) {
                assert!(row.contains(expected), "missing {expected:?} in {row:?}");
                assert!(row.starts_with('│') && row.ends_with('│'));
            }
        }
    }

    #[test]
    fn selection_is_clamped_and_cleared_with_the_data() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut list = ResourceList::new(" Test ");
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();

        terminal
            .draw(|frame| {
                list.render(
                    frame,
                    frame.area(),
                    &["first", "second"],
                    ResourceListRender {
                        selected: 8,
                        resize_mode: false,
                        empty_message: "No entries",
                        trailing_title: None,
                    },
                    |entry, styles| {
                        ListItem::new(Line::from(vec![
                            styles.cursor_span(),
                            Span::raw((*entry).to_string()),
                        ]))
                    },
                )
            })
            .unwrap();
        assert_eq!(list.state.selected(), Some(1));

        terminal
            .draw(|frame| {
                list.render(
                    frame,
                    frame.area(),
                    &[] as &[&str],
                    ResourceListRender {
                        selected: 0,
                        resize_mode: false,
                        empty_message: "No entries",
                        trailing_title: None,
                    },
                    |_, _| unreachable!(),
                )
            })
            .unwrap();
        assert_eq!(list.state.selected(), None);
    }

    #[test]
    fn mouse_rows_use_the_rendered_scroll_offset() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut list = ResourceList::new(" Test ");
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();

        terminal
            .draw(|frame| {
                list.render(
                    frame,
                    frame.area(),
                    &["first", "second", "third", "fourth", "fifth", "sixth"],
                    ResourceListRender {
                        selected: 5,
                        resize_mode: false,
                        empty_message: "No entries",
                        trailing_title: None,
                    },
                    |entry, styles| {
                        ListItem::new(Line::from(vec![
                            styles.cursor_span(),
                            Span::raw((*entry).to_string()),
                        ]))
                    },
                )
            })
            .unwrap();

        assert_eq!(
            list.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            EventResult::Message(AppMessage::List(ListMessage::Select(4)))
        );
    }

    fn render_empty(message: &str) -> Vec<String> {
        let mut list = ResourceList::new(" Test ");
        let mut terminal = Terminal::new(TestBackend::new(18, 5)).unwrap();
        terminal
            .draw(|frame| {
                list.render(
                    frame,
                    frame.area(),
                    &[] as &[()],
                    ResourceListRender {
                        selected: 0,
                        resize_mode: false,
                        empty_message: message,
                        trailing_title: None,
                    },
                    |_, _| unreachable!(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect()
            })
            .collect()
    }
}
