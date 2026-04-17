use crate::nspawn::models::CreateUser;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::ui::widgets::lists::editable_list::EditableList;
use crate::ui::widgets::composites::user_editor::UserEditor;
use crate::ui::widgets::inputs::password_box::PasswordBox;
use crate::ui::wizard::context::{UserConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::ui::wizard::core::render_editor_overlay;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.root_password, &mut $self.user_list];
        comps
    }};
}

impl_wizard_nav!(UserStepView, active_comps);

pub struct UserStepView {
    root_password: PasswordBox,
    user_list: EditableList<CreateUser>,

    editor: Option<UserEditor>,
    focus: FocusTracker,
}

impl UserStepView {
    pub fn new(initial_data: &UserConfig) -> Self {
        let users = initial_data.users.clone();

        let mut view = Self {
            root_password: PasswordBox::new(
                " Root Password (optional) ",
                initial_data.root_password.clone().unwrap_or_default(),
            ),
            user_list: EditableList::new(
                " Regular Users ",
                users,
                |u| format!("  {} {}", u.username, if u.sudoer { "[sudo]" } else { "" }),
                |idx| AppMessage::Wizard(WizardMessage::UserRemoved(idx)),
            ),

            editor: None,
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }

}

impl Component for UserStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        if let Some(editor) = &mut self.editor {
            render_editor_overlay(f, "Add/Edit User", 60, 85, editor);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3), // Root password
                Constraint::Length(1), // Spacer
                Constraint::Min(0),    // List
                Constraint::Length(1), // Hint
            ])
            .split(area);

        self.root_password.render(f, chunks[0]);
        self.user_list.render(f, chunks[2]);

        let hint = " [Tab] switch, [A]dd user, [D]elete user, [Enter] next ";
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::Yellow)),
            chunks[3],
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if let Some(editor) = &mut self.editor {
            if key.code == KeyCode::Esc {
                self.editor = None;
                return EventResult::Consumed;
            }
            let res = editor.handle_key(key);
            match &res {
                EventResult::Message(AppMessage::Wizard(WizardMessage::UserAdded(user))) => {
                    self.user_list.add_item(user.clone());
                    self.editor = None;
                    self.update_focus();
                }
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                    self.editor = None;
                    self.update_focus();
                    return EventResult::Consumed;
                }
                _ => {}
            }
            return res;
        }

        if key.code == KeyCode::Char('a') || key.code == KeyCode::Char('A') {
            if self.user_list.is_focused() {
                self.editor = Some(UserEditor::new(|u| {
                    AppMessage::Wizard(WizardMessage::UserAdded(u))
                }));

                self.editor.as_mut().unwrap().set_focus(true);
                return EventResult::Consumed;
            }
        }

        let res = delegate_wizard_navigation!(self, key, active_comps);
        if let EventResult::Message(AppMessage::Wizard(WizardMessage::UserRemoved(_))) = res {
            self.update_focus();
        }
        res
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.root_password.set_focus(false);
            self.user_list.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.root_password.is_focused() || self.user_list.is_focused()
    }

    fn validate(&mut self) -> Result<(), String> {
        self.root_password.validate()?;
        Ok(())
    }
}

impl StepComponent for UserStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.user.root_password = self.root_password.value().to_string();
        ctx.user.users = self.user_list.items().to_vec();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
