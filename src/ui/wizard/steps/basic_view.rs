use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::wizard::context::{BasicConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;
use std::collections::HashSet;

macro_rules! active_comps {
    ($self:ident) => {{
        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.name];
        if $self.show_hostname {
            comps.push(&mut $self.hostname);
        }
        comps
    }};
}

impl_wizard_nav!(BasicStepView, active_comps);

pub struct BasicStepView {
    name: TextBox,
    hostname: TextBox,
    show_hostname: bool,
    focus: FocusTracker,
}

impl BasicStepView {
    pub fn new(
        initial_data: &BasicConfig,
        existing_entries: &[crate::nspawn::models::ContainerEntry],
        show_hostname: bool,
    ) -> Self {
        let existing_names: HashSet<String> = existing_entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        let mut view = Self {
            name: TextBox::new(" Container name (required) ", initial_data.name.clone())
                .with_validator(move |value| validate_container_name(value, &existing_names)),
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
            show_hostname,
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }
}

fn validate_container_name(value: &str, existing_names: &HashSet<String>) -> Result<(), String> {
    let name = value.trim();
    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Invalid characters: use [a-zA-Z0-9_-]".to_string());
    }
    if name.len() > 64 {
        return Err("Name too long (max 64)".to_string());
    }
    if existing_names.contains(name) {
        return Err(format!("Machine image '{}' already exists", name));
    }
    Ok(())
}

impl Component for BasicStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let constraints = if self.show_hostname {
            vec![Constraint::Length(3), Constraint::Length(3)]
        } else {
            vec![Constraint::Length(3)]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        self.name.render(f, chunks[0]);
        if self.show_hostname {
            self.hostname.render(f, chunks[1]);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        delegate_wizard_navigation!(self, key, active_comps)
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }

    fn validate(&mut self) -> Result<(), String> {
        self.name.validate()?;
        if self.show_hostname {
            self.hostname.validate()?;
        }
        Ok(())
    }
}

impl StepComponent for BasicStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.basic.name = self.name.value().to_string();
        if self.show_hostname {
            ctx.basic.hostname = self.hostname.value().to_string();
        } else {
            ctx.basic.hostname.clear();
        }
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_rejects_existing_machine() {
        let existing = ["arch-test".to_string()].into_iter().collect();

        let result = validate_container_name("arch-test", &existing);

        assert_eq!(
            result,
            Err("Machine image 'arch-test' already exists".into())
        );
    }

    #[test]
    fn container_name_accepts_unique_machine() {
        let existing = ["arch-test".to_string()].into_iter().collect();

        assert!(validate_container_name("new-container", &existing).is_ok());
    }
}
