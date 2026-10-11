//! A form's single layout declaration: nested rows/columns, field identity,
//! availability and geometry. It borrows controls for the duration of a visit.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use super::{ControlInfo, FormState, Placement};
use crate::tui::foundation::widgets::control::{Control, ControlContext};

pub(super) struct Field<'a, Id> {
    pub(super) id: Id,
    pub(super) control: &'a mut dyn Control,
    pub(super) enabled: bool,
    pub(super) enter_action: Option<&'static str>,
}

enum Content<'a, Id> {
    Field(Field<'a, Id>),
    Split(Direction, Vec<FormItem<'a, Id>>),
    Spacer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutKind {
    Field,
    Split {
        direction: Direction,
        children: usize,
    },
    Spacer,
}

/// A bounded, non-borrowing description used to invalidate old pointer targets.
/// Child counts preserve nesting without sentinel values or retaining controls.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct LayoutInfo {
    kind: LayoutKind,
    height: Constraint,
    width: Constraint,
    inset_percent: u16,
}

/// A field or a nested row/column. Widths apply within rows; heights within columns.
pub(crate) struct FormItem<'a, Id> {
    content: Content<'a, Id>,
    height: Constraint,
    width: Constraint,
    inset_percent: u16,
}

impl<'a, Id> FormItem<'a, Id> {
    pub(crate) fn field(id: Id, control: &'a mut dyn Control) -> Self {
        Self {
            content: Content::Field(Field {
                id,
                control,
                enabled: true,
                enter_action: None,
            }),
            height: Constraint::Length(3),
            width: Constraint::Fill(1),
            inset_percent: 100,
        }
    }

    pub(crate) fn row(items: impl IntoIterator<Item = Self>) -> Self {
        let items: Vec<_> = items.into_iter().collect();
        let height = items.iter().map(Self::minimum_height).max().unwrap_or(0);
        Self {
            content: Content::Split(Direction::Horizontal, items),
            height: Constraint::Length(height),
            width: Constraint::Fill(1),
            inset_percent: 100,
        }
    }

    pub(crate) fn column(items: impl IntoIterator<Item = Self>) -> Self {
        Self {
            content: Content::Split(Direction::Vertical, items.into_iter().collect()),
            height: Constraint::Min(0),
            width: Constraint::Fill(1),
            inset_percent: 100,
        }
    }

    pub(crate) fn spacer() -> Self {
        Self {
            content: Content::Spacer,
            height: Constraint::Min(0),
            width: Constraint::Fill(1),
            inset_percent: 100,
        }
    }

    pub(crate) fn height(mut self, height: u16) -> Self {
        self.height = Constraint::Length(height);
        self
    }
    pub(crate) fn fill(mut self, minimum: u16) -> Self {
        self.height = Constraint::Min(minimum);
        self
    }
    pub(crate) fn with_width(mut self, percent: u16) -> Self {
        self.inset_percent = percent.min(100);
        self
    }

    pub(crate) fn with_enabled(mut self, enabled: bool) -> Self {
        if let Content::Field(field) = &mut self.content {
            field.enabled = enabled;
        }
        self
    }

    pub(crate) fn on_enter(mut self, label: &'static str) -> Self {
        if let Content::Field(field) = &mut self.content {
            field.enter_action = Some(label);
        }
        self
    }

    pub(super) fn field_ref(&self, id: Id) -> Option<&Field<'a, Id>>
    where
        Id: Copy + Eq,
    {
        match &self.content {
            Content::Field(field) => (field.id == id).then_some(field),
            Content::Split(_, items) => items.iter().find_map(|item| item.field_ref(id)),
            Content::Spacer => None,
        }
    }

    pub(super) fn field_mut(&mut self, id: Id) -> Option<&mut Field<'a, Id>>
    where
        Id: Copy + Eq,
    {
        match &mut self.content {
            Content::Field(field) => (field.id == id).then_some(field),
            Content::Split(_, items) => items.iter_mut().find_map(|item| item.field_mut(id)),
            Content::Spacer => None,
        }
    }

    pub(super) fn describe(&self, order: &mut Vec<ControlInfo<Id>>, layout: &mut Vec<LayoutInfo>)
    where
        Id: Copy,
    {
        let kind = match &self.content {
            Content::Field(_) => LayoutKind::Field,
            Content::Split(direction, items) => LayoutKind::Split {
                direction: *direction,
                children: items.len(),
            },
            Content::Spacer => LayoutKind::Spacer,
        };
        layout.push(LayoutInfo {
            kind,
            height: self.height,
            width: self.width,
            inset_percent: self.inset_percent,
        });
        match &self.content {
            Content::Field(field) => order.push(ControlInfo {
                id: field.id,
                enabled: field.enabled,
                focusable: field.enabled && field.control.is_focusable(),
            }),
            Content::Split(_, items) => {
                for item in items {
                    item.describe(order, layout);
                }
            }
            Content::Spacer => {}
        }
    }

    pub(super) fn minimum_height(&self) -> u16 {
        let content = match &self.content {
            Content::Split(Direction::Vertical, items) => items
                .iter()
                .map(Self::minimum_height)
                .fold(0, u16::saturating_add),
            Content::Split(Direction::Horizontal, items) => {
                items.iter().map(Self::minimum_height).max().unwrap_or(0)
            }
            _ => 0,
        };
        match self.height {
            Constraint::Length(height) => height,
            Constraint::Min(height) => height.max(content),
            _ => content,
        }
    }

    fn constraint(&self, direction: Direction) -> Constraint {
        if direction == Direction::Horizontal {
            self.width
        } else if matches!(self.height, Constraint::Min(_)) {
            Constraint::Min(self.minimum_height())
        } else {
            self.height
        }
    }

    pub(super) fn place(&mut self, frame: &mut Frame, area: Rect, state: &mut FormState<Id>)
    where
        Id: Copy + Eq,
    {
        match &mut self.content {
            Content::Field(field) => {
                let Some(rows) = state
                    .scroll
                    .visible_rows(usize::from(area.y)..usize::from(area.bottom()))
                else {
                    return;
                };
                let preferred =
                    ((u32::from(area.width) * u32::from(self.inset_percent)) / 100) as u16;
                let width = preferred.max(field.control.minimum_width()).min(area.width);
                let cell = Rect::new(
                    area.x + (area.width - width) / 2,
                    state
                        .viewport
                        .y
                        .saturating_add((rows.start - state.scroll.offset()) as u16),
                    width,
                    rows.len() as u16,
                )
                .intersection(state.viewport);
                if cell.is_empty() {
                    return;
                }
                field.control.render(
                    frame,
                    cell,
                    ControlContext {
                        focused: state.active && state.focus == Some(field.id),
                        enabled: field.enabled,
                    },
                );
                state.placements.push(Placement {
                    id: field.id,
                    area: cell,
                });
            }
            Content::Split(direction, items) => {
                let constraints = items.iter().map(|item| item.constraint(*direction));
                let cells = Layout::default()
                    .direction(*direction)
                    .constraints(constraints)
                    .split(area);
                for (item, cell) in items.iter_mut().zip(cells.iter()) {
                    item.place(frame, *cell, state);
                }
            }
            Content::Spacer => {}
        }
    }

    pub(super) fn focused_area(&self, area: Rect, id: Id) -> Option<Rect>
    where
        Id: Copy + Eq,
    {
        match &self.content {
            Content::Field(field) => (field.id == id).then_some(area),
            Content::Split(direction, items) => {
                let constraints = items.iter().map(|item| item.constraint(*direction));
                let cells = Layout::default()
                    .direction(*direction)
                    .constraints(constraints)
                    .split(area);
                items
                    .iter()
                    .zip(cells.iter())
                    .find_map(|(item, cell)| item.focused_area(*cell, id))
            }
            Content::Spacer => None,
        }
    }
}
