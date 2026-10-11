//! List values belong to the feature. The control borrows their presentation
//! and only keeps selection/viewport state; activation never edits those values.

use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::selection::navigate_selection;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};
use std::borrow::Cow;

const SELECTION_HINTS: &[FooterHint] = &[
    FooterHint::new("↑/↓ / j/k", "select"),
    FooterHint::new("PgUp/PgDn", "page").secondary(),
    FooterHint::new("Home/End", "first/last").secondary(),
];
const CHECKLIST_HINTS: &[FooterHint] = &[
    FooterHint::new("↑/↓ / j/k", "select"),
    FooterHint::new("Space", "toggle").on_key(KeyCode::Char(' ')),
    FooterHint::new("PgUp/PgDn", "page").secondary(),
    FooterHint::new("Home/End", "first/last").secondary(),
];

#[derive(Default)]
pub(crate) struct ListControlState {
    state: ListState,
    viewport_rows: usize,
}

impl ListControlState {
    pub(crate) fn new(selected: usize) -> Self {
        Self {
            state: ListState::default().with_selected(Some(selected)),
            viewport_rows: 0,
        }
    }

    pub(crate) fn selected(&self) -> Option<usize> {
        self.state.selected()
    }
    pub(crate) fn select(&mut self, selected: usize) {
        self.state.select(Some(selected));
    }

    pub(crate) fn removed(&mut self, index: usize, remaining: usize) {
        let next = self.selected().map(|selected| {
            selected
                .saturating_sub(usize::from(index < selected))
                .min(remaining.saturating_sub(1))
        });
        self.state.select(next.filter(|_| remaining > 0));
        if remaining == 0 {
            *self.state.offset_mut() = 0;
        }
    }
}

pub(crate) struct ListEntry<'a> {
    label: Cow<'a, str>,
    enabled: bool,
    checked: Option<bool>,
}

impl<'a> ListEntry<'a> {
    pub(crate) fn new(label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            checked: None,
        }
    }
    pub(crate) fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub(crate) fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }
}

pub(crate) struct ListControl<'a> {
    state: &'a mut ListControlState,
    label: &'a str,
    entries: &'a [ListEntry<'a>],
}

impl<'a> ListControl<'a> {
    pub(crate) fn selected(&self) -> Option<usize> {
        self.state.selected()
    }
    pub(crate) fn new(
        state: &'a mut ListControlState,
        label: &'a str,
        entries: &'a [ListEntry<'a>],
    ) -> Self {
        if state
            .selected()
            .is_none_or(|index| !entries.get(index).is_some_and(|entry| entry.enabled))
        {
            state
                .state
                .select(entries.iter().position(|entry| entry.enabled));
        }
        if entries.is_empty() {
            *state.state.offset_mut() = 0;
        }
        Self {
            state,
            label,
            entries,
        }
    }

    fn navigate(&mut self, action: ScrollAction) -> ControlOutcome {
        let previous = self.state.selected();
        self.state.state.select(navigate_selection(
            action,
            previous,
            self.entries.len(),
            self.state.viewport_rows,
            true,
            |index| self.entries[index].enabled,
        ));
        if self.state.selected() == previous {
            ControlOutcome::Consumed
        } else {
            ControlOutcome::Changed
        }
    }

    fn activate(&self) -> ControlOutcome {
        if self
            .state
            .selected()
            .and_then(|index| self.entries.get(index))
            .is_some_and(|entry| entry.enabled && entry.checked.is_some())
        {
            ControlOutcome::Activated
        } else {
            ControlOutcome::Ignored
        }
    }
}

impl Control for ListControl<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        let t = crate::tui::foundation::theme::theme();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(self.label)
            .border_style(
                Style::default().fg(crate::tui::foundation::theme::widget_border_color(
                    context.focused,
                    context.enabled,
                )),
            );
        self.state.viewport_rows = usize::from(block.inner(area).height);
        let items = self.entries.iter().map(|entry| {
            let content = if let Some(checked) = entry.checked {
                Cow::Owned(format!(
                    "[{}] {}",
                    if checked { "x" } else { " " },
                    entry.label
                ))
            } else {
                Cow::Borrowed(entry.label.as_ref())
            };
            let style = if context.enabled && entry.enabled {
                Style::default()
            } else {
                Style::default()
                    .fg(t.list_disabled_item)
                    .add_modifier(Modifier::DIM)
            };
            ListItem::new(content).style(style)
        });
        let mut list = List::new(items).block(block);
        if context.enabled {
            list = list
                .highlight_style(
                    Style::default()
                        .fg(t.list_highlight_symbol)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">> ");
        }
        frame.render_stateful_widget(list, area, &mut self.state.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        if let Some(action) = ScrollAction::from_key(key) {
            return self.navigate(action);
        }
        if key.code == KeyCode::Char(' ') {
            self.activate()
        } else {
            ControlOutcome::Ignored
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> ControlOutcome {
        let body = Block::default().borders(Borders::ALL).inner(area);
        if !body.contains((mouse.column, mouse.row).into()) {
            return ControlOutcome::Ignored;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let index = self.state.state.offset() + usize::from(mouse.row - body.y);
            if !self.entries.get(index).is_some_and(|entry| entry.enabled) {
                return ControlOutcome::Consumed;
            }
            let changed = self.state.selected() != Some(index);
            self.state.select(index);
            return if self.activate() == ControlOutcome::Activated {
                ControlOutcome::Activated
            } else if changed {
                ControlOutcome::Changed
            } else {
                ControlOutcome::Consumed
            };
        }
        if let Some(action) = ScrollAction::from_mouse(mouse.kind) {
            return self.navigate(action);
        }
        ControlOutcome::Ignored
    }

    fn hints(&self) -> &'static [FooterHint] {
        if self.entries.is_empty() {
            &[]
        } else if self.entries.iter().any(|entry| entry.checked.is_some()) {
            CHECKLIST_HINTS
        } else {
            SELECTION_HINTS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_entries_cannot_be_selected_and_removing_preserves_selection() {
        let mut state = ListControlState::new(0);
        let entries = [
            ListEntry::new("unsupported").with_enabled(false),
            ListEntry::new("supported"),
        ];
        let mut list = ListControl::new(&mut state, "Storage", &entries);
        assert_eq!(list.state.selected(), Some(1));
        list.navigate(ScrollAction::Down(1));
        assert_eq!(state.selected(), Some(1));
        state.removed(0, 1);
        assert_eq!(state.selected(), Some(0));
        let entries = [ListEntry::new("disabled").with_enabled(false)];
        ListControl::new(&mut state, "Storage", &entries);
        assert_eq!(state.selected(), None);
        ListControl::new(&mut state, "Storage", &[]);
        assert_eq!(state.state.offset(), 0);
    }

    #[test]
    fn clicking_a_selected_entry_is_not_a_value_change_and_checks_remain_owned_by_the_caller() {
        let mut state = ListControlState::new(0);
        let entries = [ListEntry::new("first"), ListEntry::new("last")];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let area = Rect::new(0, 0, 20, 5);
        let mut list = ListControl::new(&mut state, "Display", &entries);
        assert_eq!(list.handle_mouse(click, area), ControlOutcome::Consumed);
        assert_eq!(
            list.handle_mouse(MouseEvent { row: 2, ..click }, area),
            ControlOutcome::Changed
        );
        assert_eq!(list.selected(), Some(1));

        let entries = [ListEntry::new("first").checked(false)];
        let mut list = ListControl::new(&mut state, "Bindings", &entries);
        assert_eq!(list.handle_mouse(click, area), ControlOutcome::Activated);
        assert_eq!(entries[0].checked, Some(false));
    }
}
