use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::ui::widgets::checkbox::Checkbox;
use crate::nspawn::StatusLevel;
use async_trait::async_trait;

pub struct UserStep;

impl UserStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for UserStep {
    fn title(&self) -> String { "User Setup".into() }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3), // Root password [0]
                Constraint::Length(1),
                Constraint::Length(3), // Enable toggle [2]
                Constraint::Length(1),
                Constraint::Length(3), // Username [4]
                Constraint::Length(1),
                Constraint::Length(3), // User password [6]
                Constraint::Length(1),
                Constraint::Length(3), // Shell [8]
                Constraint::Length(1),
                Constraint::Length(3), // Sudo checkbox [10]
                Constraint::Min(0),
                Constraint::Length(1), // Hint [12]
            ])
            .split(area);

        let root_pwd = if context.user_field == 0 { format!("{}_", context.root_password) } else { context.root_password.clone() };
        Input::new(" Root Password (optional) ", &root_pwd)
            .focused(context.user_field == 0)
            .render(f, chunks[0]);

        Checkbox::new("Create a regular user", context.user_enabled)
            .focused(context.user_field == 1)
            .render(f, chunks[2]);

        let user_enabled = context.user_enabled;
        
        let user_text = if context.user_field == 2 { format!("{}_", context.user.username) } else { context.user.username.clone() };
        Input::new(" Username ", &user_text)
            .focused(user_enabled && context.user_field == 2)
            .render(f, chunks[4]);

        let pwd_text = if context.user_field == 3 { format!("{}_", context.user.password) } else { context.user.password.clone() };
        Input::new(" Password (optional) ", &pwd_text)
            .focused(user_enabled && context.user_field == 3)
            .render(f, chunks[6]);

        let shell_text = if context.user_field == 4 {
            let mut s = context.user.shell.clone();
            if s.is_empty() && user_enabled { s = "/bin/bash".into(); }
            format!("{}_", s)
        } else {
            if context.user.shell.is_empty() { "/bin/bash".into() } else { context.user.shell.clone() }
        };
        Input::new(" Shell ", &shell_text)
            .focused(user_enabled && context.user_field == 4)
            .render(f, chunks[8]);

        Checkbox::new("Add to sudo / wheel group", context.user.sudoer)
            .focused(user_enabled && context.user_field == 5)
            .render(f, chunks[10]);

        render_hint(f, chunks[12], &["[Space] toggle checkbox", "[Tab] switch field", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Char(' ') if context.user_field == 1 => { context.user_enabled = !context.user_enabled; StepAction::None }
            KeyCode::Tab => {
                let max_fields = if context.user_enabled { 6 } else { 2 };
                context.user_field = (context.user_field + 1) % max_fields;
                StepAction::None
            }
            KeyCode::Backspace => {
                match context.user_field {
                    0 => { context.root_password.pop(); }
                    2 => { context.user.username.pop(); }
                    3 => { context.user.password.pop(); }
                    4 => { context.user.shell.pop(); }
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Char(' ') if context.user_field == 5 => { context.user.sudoer = !context.user.sudoer; StepAction::None }
            KeyCode::Char(c) => {
                match context.user_field {
                    0 => context.root_password.push(c),
                    2 => context.user.username.push(c),
                    3 => context.user.password.push(c),
                    4 => context.user.shell.push(c),
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Enter => {
                if context.user_enabled && context.user.username.is_empty() {
                    context.user_field = 2;
                    StepAction::Status("Please enter a username or uncheck regular user".into(), StatusLevel::Error)
                } else {
                    context.user_field = 0;
                    if context.bridge_list.is_empty() {
                        context.bridge_list = crate::nspawn::create::detect_bridges();
                        if !context.bridge_list.is_empty() { context.bridge_name = context.bridge_list[0].clone(); }
                    }
                    StepAction::Next
                }
            }
            _ => StepAction::None,
        }
    }
}
