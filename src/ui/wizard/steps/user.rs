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

        let root_pwd = if context.user.field_idx == 0 { format!("{}_", context.user.root_password) } else { context.user.root_password.clone() };
        Input::new(" Root Password (optional) ", &root_pwd)
            .focused(context.user.field_idx == 0)
            .render(f, chunks[0]);

        Checkbox::new("Create a regular user", context.user.enabled)
            .focused(context.user.field_idx == 1)
            .render(f, chunks[2]);

        let user_enabled = context.user.enabled;
        
        let user_text = if context.user.field_idx == 2 { format!("{}_", context.user.user.username) } else { context.user.user.username.clone() };
        Input::new(" Username ", &user_text)
            .focused(user_enabled && context.user.field_idx == 2)
            .render(f, chunks[4]);

        let pwd_text = if context.user.field_idx == 3 { format!("{}_", context.user.user.password) } else { context.user.user.password.clone() };
        Input::new(" Password (optional) ", &pwd_text)
            .focused(user_enabled && context.user.field_idx == 3)
            .render(f, chunks[6]);

        let shell_text = if context.user.field_idx == 4 {
            let mut s = context.user.user.shell.clone();
            if s.is_empty() && user_enabled { s = "/bin/bash".into(); }
            format!("{}_", s)
        } else {
            if context.user.user.shell.is_empty() { "/bin/bash".into() } else { context.user.user.shell.clone() }
        };
        Input::new(" Shell ", &shell_text)
            .focused(user_enabled && context.user.field_idx == 4)
            .render(f, chunks[8]);

        Checkbox::new("Add to sudo / wheel group", context.user.user.sudoer)
            .focused(user_enabled && context.user.field_idx == 5)
            .render(f, chunks[10]);

        render_hint(f, chunks[12], &["[Space] toggle checkbox", "[Tab] switch field", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Char(' ') if context.user.field_idx == 1 => { context.user.enabled = !context.user.enabled; StepAction::None }
            KeyCode::Tab => {
                let max_fields = if context.user.enabled { 6 } else { 2 };
                context.user.field_idx = (context.user.field_idx + 1) % max_fields;
                StepAction::None
            }
            KeyCode::Backspace => {
                match context.user.field_idx {
                    0 => { context.user.root_password.pop(); }
                    2 => { context.user.user.username.pop(); }
                    3 => { context.user.user.password.pop(); }
                    4 => { context.user.user.shell.pop(); }
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Char(' ') if context.user.field_idx == 5 => { context.user.user.sudoer = !context.user.user.sudoer; StepAction::None }
            KeyCode::Char(c) => {
                match context.user.field_idx {
                    0 => context.user.root_password.push(c),
                    2 => context.user.user.username.push(c),
                    3 => context.user.user.password.push(c),
                    4 => context.user.user.shell.push(c),
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Enter => {
                if context.user.enabled && context.user.user.username.is_empty() {
                    context.user.field_idx = 2;
                    StepAction::Status("Please enter a username or uncheck regular user".into(), StatusLevel::Error)
                } else {
                    context.user.field_idx = 0;
                    if context.network.bridge_list.is_empty() {
                        context.network.bridge_list = crate::nspawn::create::detect_bridges();
                        if !context.network.bridge_list.is_empty() { context.network.bridge_name = context.network.bridge_list[0].clone(); }
                    }
                    StepAction::Next
                }
            }
            _ => StepAction::None,
        }
    }
}
