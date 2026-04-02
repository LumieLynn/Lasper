use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::wizard::context::{BasicConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![&mut $self.name, &mut $self.hostname];
        comps
    }};
}

pub struct BasicStepView {
    name: TextBox,
    hostname: TextBox,
    focus: FocusTracker,
}

impl BasicStepView {
    pub fn new(initial_data: &BasicConfig) -> Self {
        let mut view = Self {
            name: TextBox::new(" Container name (required) ", initial_data.name.clone())
                .with_validator(|v| {
                    let s = v.trim();
                    if s.is_empty() {
                        return Err("Name cannot be empty".to_string());
                    }
                    if !s
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        return Err("Invalid characters: use [a-zA-Z0-9_-]".to_string());
                    }
                    if s.len() > 64 {
                        return Err("Name too long (max 64)".to_string());
                    }
                    Ok(())
                }),
            hostname: TextBox::new(
                " Hostname (optional, defaults to name) ",
                initial_data.hostname.clone(),
            )
            .with_validator(|v| {
                let s = v.trim();
                if s.is_empty() {
                    return Ok(());
                }
                if !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
                {
                    return Err("Invalid hostname characters".into());
                }
                Ok(())
            }),
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }

    fn update_focus(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.update_focus(&mut comps, true);
    }

    fn next(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.next(&mut comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.prev(&mut comps);
        self.update_focus();
    }
}

impl Component for BasicStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(3)])
            .split(area);

        self.name.render(f, chunks[0]);
        self.hostname.render(f, chunks[1]);
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
            _ => {}
        }

        let mut comps = active_comps!(self);
        let res = comps[self.focus.active_idx].handle_key(key);
        match res {
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
            self.name.set_focus(false);
            self.hostname.set_focus(false);
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        self.name.validate()?;
        self.hostname.validate()?;
        Ok(())
    }
}

impl StepComponent for BasicStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.basic.name = self.name.value().to_string();
        ctx.basic.hostname = self.hostname.value().to_string();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
