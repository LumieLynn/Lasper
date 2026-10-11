use crate::application::configuration::ConfigurationTarget;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::{Component, EventResult};
use ratatui::{layout::Rect, Frame};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceAction {
    Configure { target: ConfigurationTarget },
    StartImage { image: String },
    EnableNspawnUnit { image: String },
    DisableNspawnUnit { image: String },
    DeleteImage { image: String },
    PoweroffMachine { machine: String },
    RebootMachine { machine: String },
    TerminateMachine { machine: String },
    KillMachine { machine: String },
}

struct ActionItem {
    action: ResourceAction,
    label: &'static str,
    enabled: bool,
}

pub struct ResourceActionMenu {
    label: &'static str,
    items: Vec<ActionItem>,
    selected: ListControlState,
    form: FormState<()>,
    item_count: u16,
}

impl ResourceActionMenu {
    pub fn for_machine(machine: &MachineEntry) -> Self {
        let enabled = machine.access().is_nspawn() && machine.state.accepts_runtime_actions();
        let name = machine.name.clone();
        let mut items = vec![
            ActionItem {
                action: ResourceAction::PoweroffMachine {
                    machine: name.clone(),
                },
                label: "  ⏹  Power off",
                enabled,
            },
            ActionItem {
                action: ResourceAction::RebootMachine {
                    machine: name.clone(),
                },
                label: "  ↻  Reboot",
                enabled,
            },
            ActionItem {
                action: ResourceAction::TerminateMachine {
                    machine: name.clone(),
                },
                label: "  ⚠  Terminate",
                enabled,
            },
            ActionItem {
                action: ResourceAction::KillMachine { machine: name },
                label: "  ☠  Kill (SIGKILL)",
                enabled,
            },
        ];
        if let Ok(target) = ConfigurationTarget::for_machine(machine) {
            items.push(ActionItem {
                action: ResourceAction::Configure { target },
                label: "  Configure",
                enabled: true,
            });
        }
        Self::new(" [ Machine Actions ] ", items)
    }

    pub fn for_image(image: &ImageEntry, machine_present: bool, removing: bool) -> Self {
        let launchable = !image.is_hidden() && MachineName::new(&image.name).is_ok();
        let unit_mutable = launchable && !removing;
        let removable =
            !ImageEntry::is_protected_name(&image.name) && !machine_present && !removing;
        let name = image.name.clone();
        let mut items = vec![
            ActionItem {
                action: ResourceAction::StartImage {
                    image: name.clone(),
                },
                label: "  ▶  Start image",
                enabled: launchable && !machine_present && !removing,
            },
            ActionItem {
                action: ResourceAction::EnableNspawnUnit {
                    image: name.clone(),
                },
                label: "  ↑  Enable at boot",
                enabled: unit_mutable,
            },
            ActionItem {
                action: ResourceAction::DisableNspawnUnit {
                    image: name.clone(),
                },
                label: "  ↓  Disable at boot",
                enabled: unit_mutable,
            },
            ActionItem {
                action: ResourceAction::DeleteImage { image: name },
                label: "  ✕  Delete image",
                enabled: removable,
            },
        ];
        if let Ok(target) = ConfigurationTarget::for_image(image) {
            items.push(ActionItem {
                action: ResourceAction::Configure { target },
                label: "  Configure",
                enabled: true,
            });
        }
        Self::new(" [ Image Actions ] ", items)
    }

    fn new(label: &'static str, items: Vec<ActionItem>) -> Self {
        let item_count = items.len() as u16;
        let mut menu = Self {
            label,
            items,
            selected: ListControlState::new(0),
            form: FormState::new(()),
            item_count,
        };
        menu.form.set_focus(true);
        menu.with_form(|_| ());
        menu
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, ()>) -> R) -> R {
        let entries: Vec<_> = self
            .items
            .iter()
            .map(|item| ListEntry::new(item.label).with_enabled(item.enabled))
            .collect();
        let mut list = ListControl::new(&mut self.selected, self.label, &entries);
        visit(&mut Form::new(
            &mut self.form,
            [FormItem::field((), &mut list).fill(4)],
        ))
    }

    pub fn selected_action(&self) -> Option<ResourceAction> {
        self.selected
            .selected()
            .and_then(|index| self.items.get(index))
            .filter(|item| item.enabled)
            .map(|item| item.action.clone())
    }
}

impl Component for ResourceActionMenu {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        footer.register(FooterHint::new("j/k", "select").primary());
        if self.selected_action().is_some() {
            footer
                .register(FooterHint::new("Enter", "run").on_key(crossterm::event::KeyCode::Enter));
        }
        footer.register(FooterHint::new("Esc", "close").on_key(crossterm::event::KeyCode::Esc));
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let menu_height = self.item_count.saturating_add(2);
        let menu_width = 32;
        let x = area.x + (area.width.saturating_sub(menu_width)) / 2;
        let y = area.y + (area.height.saturating_sub(menu_height)) / 2;
        let area = Rect::new(
            x,
            y,
            menu_width.min(area.width),
            menu_height.min(area.height),
        );

        f.render_widget(ratatui::widgets::Clear, area);
        self.with_form(|form| form.render(f, area));
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult {
        match self.with_form(|form| form.handle_key(key)) {
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> EventResult {
        match self.with_form(|form| form.handle_mouse(mouse)) {
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::MachineState;

    fn image(name: &str) -> ImageEntry {
        ImageEntry {
            name: name.into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        }
    }

    #[test]
    fn image_actions_follow_image_and_machine_identity_rules() {
        let menu = ResourceActionMenu::for_image(&image("ubuntu"), false, false);
        assert_eq!(
            menu.selected_action(),
            Some(ResourceAction::StartImage {
                image: "ubuntu".into()
            })
        );

        let menu = ResourceActionMenu::for_image(&image("image with spaces"), false, false);
        assert_eq!(
            menu.selected_action(),
            Some(ResourceAction::DeleteImage {
                image: "image with spaces".into()
            })
        );

        let menu = ResourceActionMenu::for_image(&image("ubuntu"), true, false);
        assert_eq!(
            menu.selected_action(),
            Some(ResourceAction::EnableNspawnUnit {
                image: "ubuntu".into()
            })
        );

        // Hidden images may still have image actions, but no Configure row.
        let internal = ResourceActionMenu::for_image(&image(".internal"), false, false);
        assert_eq!(internal.item_count, 4);
    }

    #[test]
    fn transitioning_machine_can_still_inspect_configuration() {
        let machine = MachineEntry::optimistic_nspawn("ubuntu", MachineState::Exiting);
        assert_eq!(
            ResourceActionMenu::for_machine(&machine).selected_action(),
            Some(ResourceAction::Configure {
                target: ConfigurationTarget::for_machine(&machine).unwrap()
            })
        );
    }

    #[test]
    fn foreign_machine_has_no_nspawn_runtime_actions() {
        let machine = MachineEntry {
            name: "ubuntu-vm".into(),
            class: "vm".into(),
            service: "systemd-vmspawn".into(),
            state: MachineState::Running,
            addresses: Default::default(),
        };
        assert_eq!(
            ResourceActionMenu::for_machine(&machine).selected_action(),
            None
        );
    }
}
