use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use crate::tui::core::{Component, EventResult, FocusTracker};
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::wizard::draft::{BasicConfig, WizardDraft};
use crate::tui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;
use std::collections::HashSet;

macro_rules! active_comps {
    ($self:ident) => {{
        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.name];
        if $self.show_hostname {
            comps.push(&mut $self.guest_hostname);
        }
        comps
    }};
}

impl_wizard_nav!(BasicStepView, active_comps);

pub struct BasicStepView {
    name: TextBox,
    guest_hostname: TextBox,
    show_hostname: bool,
    focus: FocusTracker,
}

impl BasicStepView {
    pub fn new(
        initial_data: &BasicConfig,
        existing_entries: &[MachineEntry],
        show_hostname: bool,
    ) -> Self {
        let existing_names: HashSet<String> = existing_entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        let mut view = Self {
            name: TextBox::new(" Machine name (required) ", initial_data.name.clone())
                .with_validator(move |value| validate_machine_name(value, &existing_names)),
            guest_hostname: TextBox::new(
                " Guest hostname (optional, defaults to machine name) ",
                initial_data.guest_hostname.clone(),
            )
            .with_validator(|value| {
                crate::domain::machine::GuestHostname::validate_optional(value)
                    .map_err(|error| error.to_string())
            }),
            show_hostname,
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }
}

fn validate_machine_name(value: &str, existing_names: &HashSet<String>) -> Result<(), String> {
    let name = value.trim();
    MachineName::new(name).map_err(|error| error.to_string())?;
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
            self.guest_hostname.render(f, chunks[1]);
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
            self.guest_hostname.validate()?;
        }
        Ok(())
    }
}

impl StepComponent for BasicStepView {
    fn commit_to_draft(&self, ctx: &mut WizardDraft) {
        ctx.basic.name = self.name.value().to_string();
        if self.show_hostname {
            ctx.basic.guest_hostname = self.guest_hostname.value().to_string();
        } else {
            ctx.basic.guest_hostname.clear();
        }
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_name_rejects_existing_machine() {
        let existing = ["arch-test".to_string()].into_iter().collect();

        let result = validate_machine_name("arch-test", &existing);

        assert_eq!(
            result,
            Err("Machine image 'arch-test' already exists".into())
        );
    }

    #[test]
    fn machine_name_accepts_unique_name() {
        let existing = ["arch-test".to_string()].into_iter().collect();

        assert!(validate_machine_name("new-container", &existing).is_ok());
    }

    #[test]
    fn machine_name_uses_the_systemd_nspawn_domain_rules() {
        let existing = HashSet::new();

        assert!(validate_machine_name("name_with_underscore", &existing).is_err());
        assert!(validate_machine_name("-leading", &existing).is_err());
        assert!(validate_machine_name("machine.example", &existing).is_ok());
    }
}
