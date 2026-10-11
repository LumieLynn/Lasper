//! Borrowed controls share one layout declaration for drawing, focus, pointer
//! routing and hints. Feature views own their values and interpret local events.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, Frame};

use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::scrolling::{ScrollAction, ScrollState};
use crate::tui::foundation::widgets::control::ControlOutcome;
use crate::tui::foundation::widgets::display::scrollbar::VerticalScrollbar;

mod layout;
#[cfg(test)]
mod tests;

pub(crate) use layout::FormItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormOutcome<Id> {
    Ignored,
    Consumed,
    Changed(Id),
    Activated(Id),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ControlInfo<Id> {
    id: Id,
    enabled: bool,
    focusable: bool,
}

struct Placement<Id> {
    id: Id,
    area: Rect,
}

/// Only navigation and the last draw's geometry survive between visits.
pub(crate) struct FormState<Id> {
    focus: Option<Id>,
    active: bool,
    order: Vec<ControlInfo<Id>>,
    layout: Vec<layout::LayoutInfo>,
    placements: Vec<Placement<Id>>,
    viewport: Rect,
    scroll: ScrollState,
    scrollbar: VerticalScrollbar,
    reveal_focus: bool,
    text_input: bool,
    hints: Vec<FooterHint>,
}

impl<Id: Copy + Eq> FormState<Id> {
    pub(crate) fn new(focus: Id) -> Self {
        Self {
            focus: Some(focus),
            active: false,
            order: Vec::new(),
            layout: Vec::new(),
            placements: Vec::new(),
            viewport: Rect::default(),
            scroll: ScrollState::default(),
            scrollbar: VerticalScrollbar::default(),
            reveal_focus: true,
            text_input: false,
            hints: Vec::new(),
        }
    }

    pub(crate) fn set_focus(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.clear_mouse_area();
        }
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.active
    }

    pub(crate) fn is_selected(&self, id: Id) -> bool {
        self.active && self.focus == Some(id)
    }

    pub(crate) fn select(&mut self, focus: Id) {
        self.select_optional(Some(focus));
    }

    pub(crate) fn captures_text_input(&self) -> bool {
        self.active && self.text_input
    }

    pub(crate) fn viewport_width(&self) -> u16 {
        self.viewport.width
    }

    #[cfg(test)]
    pub(crate) fn control_area(&self, id: Id) -> Option<Rect> {
        self.placements
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.area)
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if self.active {
            for hint in &self.hints {
                footer.register(*hint);
            }
            if self.scroll.max_offset() > 0 {
                footer.register(FooterHint::new("PgUp/PgDn", "page").secondary());
            }
        }
    }

    pub(crate) fn clear_mouse_area(&mut self) {
        self.placements.clear();
        self.scrollbar.clear_mouse_area();
    }

    fn select_optional(&mut self, focus: Option<Id>) {
        if self.focus != focus {
            self.focus = focus;
            self.reveal_focus = true;
        }
    }
}

pub(crate) struct Form<'a, Id> {
    state: &'a mut FormState<Id>,
    layout: FormItem<'a, Id>,
    actions: Option<(Id, Id)>,
}

impl<'a, Id: Copy + Eq> Form<'a, Id> {
    pub(crate) fn new(
        state: &'a mut FormState<Id>,
        items: impl IntoIterator<Item = FormItem<'a, Id>>,
    ) -> Self {
        let mut form = Self {
            state,
            layout: FormItem::column(items),
            actions: None,
        };
        form.synchronize();
        form
    }

    /// Dialog submission is explicit; a wizard step leaves Enter and Esc to its owner.
    pub(crate) fn with_actions(mut self, submit: Id, cancel: Id) -> Self {
        self.actions = Some((submit, cancel));
        self.refresh_hints();
        self
    }

    pub(crate) fn minimum_height(&self) -> u16 {
        self.layout.minimum_height()
    }

    fn synchronize(&mut self) {
        let mut order = Vec::new();
        let mut layout = Vec::new();
        self.layout.describe(&mut order, &mut layout);
        if order != self.state.order || layout != self.state.layout {
            self.state.clear_mouse_area();
            let previous = self
                .state
                .order
                .iter()
                .position(|entry| Some(entry.id) == self.state.focus)
                .unwrap_or(0);
            if !order
                .iter()
                .any(|entry| Some(entry.id) == self.state.focus && entry.focusable)
            {
                let next = order
                    .iter()
                    .skip(previous)
                    .find(|entry| entry.focusable)
                    .or_else(|| {
                        order
                            .iter()
                            .take(previous)
                            .rev()
                            .find(|entry| entry.focusable)
                    })
                    .map(|entry| entry.id);
                self.state.select_optional(next);
            }
            self.state.order = order;
            self.state.layout = layout;
        }
        self.refresh_hints();
    }

    fn refresh_hints(&mut self) {
        self.state.hints.clear();
        self.state.text_input = false;
        if self
            .state
            .order
            .iter()
            .filter(|entry| entry.focusable)
            .count()
            > 1
        {
            self.state
                .hints
                .push(FooterHint::new("Tab/⇧Tab", "fields").on_key(KeyCode::Tab));
        }
        let field = self
            .state
            .focus
            .and_then(|id| self.layout.field_ref(id))
            .filter(|field| field.enabled);
        if let Some(field) = field {
            self.state.text_input = field.control.captures_text_input();
            if let Some(label) = field.enter_action {
                self.state
                    .hints
                    .push(FooterHint::new("Enter", label).on_key(KeyCode::Enter));
            }
        }
        if let Some((submit, cancel)) = self.actions {
            let cancel_focused = self.state.focus == Some(cancel);
            if self.can_activate(if cancel_focused { cancel } else { submit })
                && !field.is_some_and(|field| field.enter_action.is_some())
            {
                self.state.hints.push(
                    FooterHint::new("Enter", if cancel_focused { "cancel" } else { "confirm" })
                        .on_key(KeyCode::Enter),
                );
            }
            if self.can_activate(cancel) {
                self.state.hints.push(
                    FooterHint::new("Esc", "cancel")
                        .primary()
                        .on_key(KeyCode::Esc),
                );
            }
        }
        if let Some(field) = field {
            self.state.hints.extend_from_slice(field.control.hints());
        }
    }

    fn can_activate(&self, id: Id) -> bool {
        self.layout
            .field_ref(id)
            .is_some_and(|field| field.enabled && field.control.is_focusable())
    }

    fn activate(&self, id: Id) -> FormOutcome<Id> {
        if self.can_activate(id) {
            FormOutcome::Activated(id)
        } else {
            FormOutcome::Consumed
        }
    }

    fn navigate(&mut self, forward: bool) {
        let order = &self.state.order;
        let current = order
            .iter()
            .position(|entry| Some(entry.id) == self.state.focus);
        let len = order.len();
        let next = (1..=len).find_map(|step| {
            let index = if forward {
                current.map_or(step - 1, |index| (index + step) % len)
            } else {
                current.map_or(len - step, |index| (index + len - step) % len)
            };
            order[index].focusable.then_some(order[index].id)
        });
        self.state.select_optional(next);
        self.refresh_hints();
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> FormOutcome<Id> {
        if !self.state.active || key.kind == KeyEventKind::Release {
            return FormOutcome::Ignored;
        }
        match key.code {
            KeyCode::Tab => {
                self.navigate(true);
                return FormOutcome::Consumed;
            }
            KeyCode::BackTab => {
                self.navigate(false);
                return FormOutcome::Consumed;
            }
            KeyCode::Esc => {
                if let Some((_, cancel)) = self.actions {
                    return self.activate(cancel);
                }
            }
            _ => {}
        }
        let Some(id) = self.state.focus else {
            return FormOutcome::Ignored;
        };
        let Some(field) = self
            .layout
            .field_mut(id)
            .filter(|field| field.enabled && field.control.is_focusable())
        else {
            return FormOutcome::Ignored;
        };
        if key.code == KeyCode::Enter && field.enter_action.is_some() {
            return FormOutcome::Activated(id);
        }
        let outcome = field.control.handle_key(key);
        if outcome == ControlOutcome::Ignored {
            if key.code == KeyCode::Enter {
                if let Some((submit, _)) = self.actions {
                    return self.activate(submit);
                }
            }
            if let Some(action) = ScrollAction::from_key(key) {
                return self.scroll(action);
            }
        } else {
            self.state.reveal_focus = true;
        }
        self.refresh_hints();
        Self::outcome(id, outcome)
    }

    fn scroll(&mut self, action: ScrollAction) -> FormOutcome<Id> {
        if self.state.scroll.max_offset() == 0 {
            return FormOutcome::Ignored;
        }
        self.state.scroll.apply(action);
        self.state.clear_mouse_area();
        FormOutcome::Consumed
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> FormOutcome<Id> {
        if !self.state.active {
            return FormOutcome::Ignored;
        }
        if self
            .state
            .scrollbar
            .handle_mouse(mouse, &mut self.state.scroll)
            == crate::tui::protocol::EventResult::Consumed
        {
            self.state.clear_mouse_area();
            return FormOutcome::Consumed;
        }
        let position = (mouse.column, mouse.row).into();
        let placement = self
            .state
            .placements
            .iter()
            .find(|entry| entry.area.contains(position))
            .map(|entry| (entry.id, entry.area));
        if let Some((id, area)) = placement {
            if let Some(field) = self
                .layout
                .field_mut(id)
                .filter(|field| field.enabled && field.control.is_focusable())
            {
                let clicked = mouse.kind == MouseEventKind::Down(MouseButton::Left);
                let outcome = field.control.handle_mouse(mouse, area);
                if clicked {
                    self.state.select_optional(Some(id));
                }
                self.refresh_hints();
                if outcome != ControlOutcome::Ignored {
                    return Self::outcome(id, outcome);
                }
                if clicked {
                    return FormOutcome::Consumed;
                }
            }
        }
        if self.state.viewport.contains(position) {
            if let Some(action) = ScrollAction::from_mouse(mouse.kind) {
                return self.scroll(action);
            }
        }
        FormOutcome::Ignored
    }

    fn outcome(id: Id, outcome: ControlOutcome) -> FormOutcome<Id> {
        match outcome {
            ControlOutcome::Ignored => FormOutcome::Ignored,
            ControlOutcome::Consumed => FormOutcome::Consumed,
            ControlOutcome::Changed => FormOutcome::Changed(id),
            ControlOutcome::Activated => FormOutcome::Activated(id),
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.intersection(frame.area());
        let resized = self.state.viewport != area;
        self.state.viewport = area;
        self.state.clear_mouse_area();
        let height = self.minimum_height().max(area.height);
        self.state
            .scroll
            .update(usize::from(height), usize::from(area.height));
        let width = area
            .width
            .saturating_sub(u16::from(self.state.scroll.max_offset() > 0));
        let virtual_area = Rect::new(area.x, 0, width, height);
        if resized || self.state.reveal_focus {
            if let Some(focused) = self
                .state
                .focus
                .and_then(|id| self.layout.focused_area(virtual_area, id))
            {
                self.state
                    .scroll
                    .reveal(usize::from(focused.y)..usize::from(focused.bottom()));
            }
            self.state.reveal_focus = false;
        }
        self.layout.place(frame, virtual_area, self.state);
        self.state.scrollbar.render(
            frame,
            Rect::new(
                area.right().saturating_sub(1),
                area.y,
                u16::from(area.width > 0),
                area.height,
            ),
            &self.state.scroll,
        );
        self.refresh_hints();
    }
}
