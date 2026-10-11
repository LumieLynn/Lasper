use crate::domain::machine::{GuestHostname, MachineName};
use crate::domain::runtime::MachineEntry;
use crate::tui::features::wizard::draft::{BasicConfig, WizardDraft};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::widgets::inputs::text_input::{Input, TextInput};
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    Frame,
};
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BasicControl {
    Name,
    Hostname,
}
pub struct BasicStepView {
    name: Input,
    guest_hostname: Input,
    name_error: Option<String>,
    hostname_error: Option<String>,
    existing_names: HashSet<String>,
    show_hostname: bool,
    form: FormState<BasicControl>,
}
impl BasicStepView {
    pub fn new(initial: &BasicConfig, existing: &[MachineEntry], show_hostname: bool) -> Self {
        let mut view = Self {
            name: Input::from(initial.name.clone()),
            guest_hostname: Input::from(initial.guest_hostname.clone()),
            name_error: None,
            hostname_error: None,
            existing_names: existing.iter().map(|entry| entry.name.clone()).collect(),
            show_hostname,
            form: FormState::new(BasicControl::Name),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, BasicControl>) -> R) -> R {
        let mut name = TextInput::new(
            &mut self.name,
            " Machine name (required) ",
            self.name_error.as_deref(),
        );
        let mut hostname = TextInput::new(
            &mut self.guest_hostname,
            " Guest hostname (optional, defaults to machine name) ",
            self.hostname_error.as_deref(),
        );
        let mut items = vec![FormItem::field(BasicControl::Name, &mut name)];
        if self.show_hostname {
            items.push(FormItem::field(BasicControl::Hostname, &mut hostname));
        }
        items.push(FormItem::spacer());
        visit(&mut Form::new(&mut self.form, items))
    }
}
fn validate_machine_name(value: &str, existing_names: &HashSet<String>) -> Result<(), String> {
    let name = value.trim();
    MachineName::new(name).map_err(|error| error.to_string())?;
    if existing_names.contains(name) {
        return Err(format!("Machine image '{name}' already exists"));
    }
    Ok(())
}
impl Component for BasicStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_mouse(mouse)))
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        self.name_error = validate_machine_name(self.name.value(), &self.existing_names).err();
        self.hostname_error = if self.show_hostname {
            GuestHostname::validate_optional(self.guest_hostname.value())
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        if let Some(error) = self.name_error.as_ref().or(self.hostname_error.as_ref()) {
            Err(error.clone())
        } else {
            Ok(())
        }
    }
}
impl StepComponent for BasicStepView {
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        context.basic.name = self.name.value().to_string();
        context.basic.guest_hostname = if self.show_hostname {
            self.guest_hostname.value().to_string()
        } else {
            String::new()
        };
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
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
