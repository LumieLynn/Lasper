use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Modifier},
    text::{Line, Span},
    widgets::{ListItem, Paragraph},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::ui::widgets::checkbox::Checkbox;
use crate::ui::widgets::list::ScrollableList;
use crate::nspawn::models::CreateUser;
use crate::nspawn::StatusLevel;
use async_trait::async_trait;

pub struct UserStep;

impl UserStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for UserStep {
    fn title(&self) -> String { "User Setup".into() }

    fn next_step(&self, _context: &WizardContext) -> Option<Box<dyn IStep>> {
        Some(Box::new(crate::ui::wizard::steps::network::NetworkStep::new()))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        if context.user.is_editing {
            self.render_edit(f, area, context);
        } else {
            self.render_main(f, area, context);
        }
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        if context.user.is_editing {
            self.handle_key_edit(key, context).await
        } else {
            self.handle_key_main(key, context).await
        }
    }
}

impl UserStep {
    fn render_main(&self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3), // Root password
                Constraint::Length(1),
                Constraint::Length(1), // Users label
                Constraint::Min(5),    // Users list
                Constraint::Length(1), // Hint
            ])
            .split(area);

        let root_pwd = if context.user.field_idx == 0 { format!("{}_", context.user.root_password) } else { context.user.root_password.clone() };
        Input::new(" Root Password (optional) ", &root_pwd)
            .focused(context.user.field_idx == 0)
            .render(f, chunks[0]);

        let p = Paragraph::new(" Additional Users:").style(Style::default().fg(Color::Cyan));
        f.render_widget(p, chunks[2]);

        let mut items = vec![ListItem::new(Line::from(vec![
            Span::styled(" >> Add new user", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ]))];

        for user in &context.user.users {
            let sudo_marker = if user.sudoer { " [sudo]" } else { "" };
            items.push(ListItem::new(Line::from(vec![
                Span::raw(format!(" {} {}", user.username, sudo_marker)),
            ])));
        }

        let selected = if context.user.field_idx == 1 { Some(context.user.user_cursor) } else { None };

        ScrollableList::new("Regular Users", items)
            .selected(selected)
            .render(f, chunks[3]);

        render_hint(f, chunks[4], &["[Tab] switch field", "[Enter] Add/Edit/Next", "[Del] remove user", "[Esc] back"][..]);
    }

    fn render_edit(&self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3), // Username
                Constraint::Length(1),
                Constraint::Length(3), // Password
                Constraint::Length(1),
                Constraint::Length(3), // Shell
                Constraint::Length(1),
                Constraint::Length(3), // Sudo
                Constraint::Min(0),
                Constraint::Length(1), // Hint
            ])
            .split(area);

        let eu = &context.user.editing_user;

        let user_text = if context.user.edit_field_idx == 0 { format!("{}_", eu.username) } else { eu.username.clone() };
        Input::new(" Username ", &user_text)
            .focused(context.user.edit_field_idx == 0)
            .render(f, chunks[0]);

        let pwd_text = if context.user.edit_field_idx == 1 { format!("{}_", eu.password) } else { eu.password.clone() };
        Input::new(" Password (optional) ", &pwd_text)
            .focused(context.user.edit_field_idx == 1)
            .render(f, chunks[2]);

        let shell_text = if context.user.edit_field_idx == 2 {
            let s = if eu.shell.is_empty() { "/bin/bash" } else { &eu.shell };
            format!("{}_", s)
        } else {
            if eu.shell.is_empty() { "/bin/bash".into() } else { eu.shell.clone() }
        };
        Input::new(" Shell ", &shell_text)
            .focused(context.user.edit_field_idx == 2)
            .render(f, chunks[4]);

        Checkbox::new("Add to sudo / wheel group", eu.sudoer)
            .focused(context.user.edit_field_idx == 3)
            .render(f, chunks[6]);

        render_hint(f, chunks[8], &["[Space] toggle checkbox", "[Tab] switch field", "[Enter] save user", "[Esc] cancel"][..]);
    }

    async fn handle_key_main(&self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Tab => {
                context.user.field_idx = (context.user.field_idx + 1) % 2;
                StepAction::None
            }
            KeyCode::Backspace if context.user.field_idx == 0 => {
                context.user.root_password.pop();
                StepAction::None
            }
            KeyCode::Char(c) if context.user.field_idx == 0 => {
                context.user.root_password.push(c);
                StepAction::None
            }
            KeyCode::Up if context.user.field_idx == 1 => {
                if context.user.user_cursor > 0 {
                    context.user.user_cursor -= 1;
                }
                StepAction::None
            }
            KeyCode::Down if context.user.field_idx == 1 => {
                let max = context.user.users.len();
                if context.user.user_cursor < max {
                    context.user.user_cursor += 1;
                }
                StepAction::None
            }
            KeyCode::Delete | KeyCode::Backspace if context.user.field_idx == 1 => {
                if context.user.user_cursor > 0 {
                    let idx = context.user.user_cursor - 1;
                    if idx < context.user.users.len() {
                        context.user.users.remove(idx);
                        if context.user.user_cursor > context.user.users.len() {
                            context.user.user_cursor = context.user.users.len();
                        }
                    }
                }
                StepAction::None
            }
            KeyCode::Enter => {
                if context.user.field_idx == 0 {
                    if context.network.bridge_list.is_empty() {
                        context.network.bridge_list = crate::nspawn::create::detect_bridges();
                        if !context.network.bridge_list.is_empty() { context.network.bridge_name = context.network.bridge_list[0].clone(); }
                    }
                    StepAction::Next
                } else {
                    if context.user.user_cursor == 0 {
                        // Add new user
                        context.user.editing_user = CreateUser {
                            username: String::new(),
                            password: String::new(),
                            shell: "/bin/bash".into(),
                            sudoer: false,
                        };
                        context.user.editing_idx = None;
                        context.user.edit_field_idx = 0;
                        context.user.is_editing = true;
                    } else {
                        // Edit existing
                        let idx = context.user.user_cursor - 1;
                        if let Some(user) = context.user.users.get(idx) {
                            context.user.editing_user = user.clone();
                            context.user.editing_idx = Some(idx);
                            context.user.edit_field_idx = 0;
                            context.user.is_editing = true;
                        }
                    }
                    StepAction::None
                }
            }
            _ => StepAction::None,
        }
    }

    async fn handle_key_edit(&self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => {
                context.user.is_editing = false;
                StepAction::None
            }
            KeyCode::Tab => {
                context.user.edit_field_idx = (context.user.edit_field_idx + 1) % 4;
                StepAction::None
            }
            KeyCode::Char(' ') if context.user.edit_field_idx == 3 => {
                context.user.editing_user.sudoer = !context.user.editing_user.sudoer;
                StepAction::None
            }
            KeyCode::Backspace => {
                match context.user.edit_field_idx {
                    0 => { context.user.editing_user.username.pop(); }
                    1 => { context.user.editing_user.password.pop(); }
                    2 => { context.user.editing_user.shell.pop(); }
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Char(c) => {
                match context.user.edit_field_idx {
                    0 => context.user.editing_user.username.push(c),
                    1 => context.user.editing_user.password.push(c),
                    2 => context.user.editing_user.shell.push(c),
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Enter => {
                let eu = &context.user.editing_user;
                if eu.username.is_empty() {
                    context.user.edit_field_idx = 0;
                    return StepAction::Status("Please enter a username".into(), StatusLevel::Error);
                }
                
                let to_save = eu.clone();
                if let Some(idx) = context.user.editing_idx {
                    context.user.users[idx] = to_save;
                } else {
                    context.user.users.push(to_save);
                }
                context.user.is_editing = false;
                StepAction::None
            }
            _ => StepAction::None,
        }
    }
}
