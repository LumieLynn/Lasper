use crate::domain::nvidia::NvidiaFileCategory;
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::lists::selectable_list::SelectableList;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use crate::tui::wizard::core::draft::UnclassifiedFile;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![
            &mut $self.destination,
            &mut $self.category_list,
            &mut $self.readonly,
            &mut $self.btn_ok,
            &mut $self.btn_cancel,
        ];
        comps
    }};
}

pub struct UnclassifiedFileDialog {
    host_path: String,
    destination: TextBox,
    category_list: SelectableList<NvidiaFileCategory>,
    readonly: Checkbox,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(UnclassifiedFile) -> AppMessage>,
}

impl UnclassifiedFileDialog {
    pub fn new(
        file: UnclassifiedFile,
        on_submit: impl Fn(UnclassifiedFile) -> AppMessage + 'static,
    ) -> Self {
        let categories = NvidiaFileCategory::all();
        let selected_cat_idx = file
            .assigned_category
            .as_ref()
            .and_then(|c| categories.iter().position(|cat| cat == c))
            .unwrap_or(categories.len() - 1); // default to Other (last)
        let mut category_list =
            SelectableList::new(" Category ", categories, |c| format!("  {}", c.label()));
        category_list.select(selected_cat_idx);

        let dest = if file.custom_destination.is_empty() {
            file.default_container_path.clone()
        } else {
            file.custom_destination.clone()
        };

        Self {
            host_path: file.host_path.clone(),
            destination: TextBox::new(" Destination Path ", dest),
            category_list,
            readonly: Checkbox::new(" Read-only", file.readonly),
            btn_ok: Button::new("OK", || AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", || AppMessage::Wizard(WizardMessage::DialogCancel)),
            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        }
    }

    fn update_focus(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.update_focus(&mut comps, true);
    }

    fn next(&mut self) {
        let comps = active_comps!(self);
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let comps = active_comps!(self);
        self.focus.prev(&comps);
        self.update_focus();
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        let dest = self.destination.value().trim().to_string();
        if dest.is_empty() || !dest.starts_with('/') {
            return None;
        }
        let assigned_category = self.category_list.selected_item().cloned();
        let readonly = self.readonly.checked();
        Some((self.on_submit)(UnclassifiedFile {
            host_path: self.host_path.clone(),
            default_container_path: String::new(), // unused on return
            assigned_category,
            custom_destination: dest,
            readonly,
        }))
    }
}

impl Component for UnclassifiedFileDialog {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let dialog_area = crate::tui::centered_rect(50, 60, area);
        f.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Reclassify File ")
            .border_style(Style::default().fg(crate::tui::theme::theme().dialog_border));
        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        let constraints = vec![
            Constraint::Length(3), // host_path display
            Constraint::Length(3), // destination
            Constraint::Min(0),    // category list (takes remaining space)
            Constraint::Length(3), // readonly
            Constraint::Length(3), // buttons
        ];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        // Host path: read-only display
        let host_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Host Path ")
            .border_style(Style::default().fg(crate::tui::theme::theme().dialog_host_border));
        let host_inner = host_block.inner(chunks[0]);
        f.render_widget(host_block, chunks[0]);
        f.render_widget(
            Paragraph::new(self.host_path.as_str())
                .style(Style::default().fg(crate::tui::theme::theme().dialog_host_text)),
            host_inner,
        );

        self.destination.render(f, chunks[1]);
        self.category_list.render(f, chunks[2]);
        self.readonly.render(f, chunks[3]);

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[4]);

        let ok_area = crate::tui::centered_rect(60, 100, btn_chunks[0]);
        let cancel_area = crate::tui::centered_rect(60, 100, btn_chunks[1]);
        self.btn_ok.render(f, ok_area);
        self.btn_cancel.render(f, cancel_area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Tab => {
                self.next();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                self.prev();
                return EventResult::Consumed;
            }
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                return if let Some(msg) = self.try_submit() {
                    EventResult::Message(msg)
                } else {
                    EventResult::Consumed
                };
            }
            _ => {}
        }

        let mut comps = active_comps!(self);
        let res = comps[self.focus.active_idx].handle_key(key);
        match res {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
                if let Some(msg) = self.try_submit() {
                    EventResult::Message(msg)
                } else {
                    EventResult::Consumed
                }
            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => res,
            EventResult::FocusNext => {
                self.next();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                self.prev();
                EventResult::Consumed
            }
            _ => res,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.destination.set_focus(false);
            self.category_list.set_focus(false);
            self.readonly.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.destination.is_focused()
            || self.category_list.is_focused()
            || self.readonly.is_focused()
            || self.btn_ok.is_focused()
            || self.btn_cancel.is_focused()
    }
}
