use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::tui::core::{Component, EventResult};
use crate::tui::widgets::lists::selectable_list::SelectableList;
use ratatui::{layout::Rect, Frame};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceAction {
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
    list: SelectableList<ActionItem>,
    item_count: u16,
}

impl ResourceActionMenu {
    pub fn for_machine(machine: &MachineEntry) -> Self {
        let enabled = machine.access().is_nspawn() && machine.state.accepts_runtime_actions();
        let name = machine.name.clone();
        Self::new(
            " [ Machine Actions ] ",
            vec![
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
            ],
        )
    }

    pub fn for_image(image: &ImageEntry, machine_present: bool, removing: bool) -> Self {
        let launchable = !image.is_hidden() && MachineName::new(&image.name).is_ok();
        let unit_mutable = launchable && !removing;
        let removable =
            !ImageEntry::is_protected_name(&image.name) && !machine_present && !removing;
        let name = image.name.clone();
        Self::new(
            " [ Image Actions ] ",
            vec![
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
            ],
        )
    }

    fn new(label: &'static str, items: Vec<ActionItem>) -> Self {
        let item_count = items.len() as u16;
        let mut list = SelectableList::new(label, items, |item| item.label.to_string())
            .with_item_enablement(|item| item.enabled);
        list.select(0);
        Self { list, item_count }
    }

    pub fn selected_action(&self) -> Option<ResourceAction> {
        self.list
            .selected_item()
            .filter(|item| item.enabled)
            .map(|item| item.action.clone())
    }
}

impl Component for ResourceActionMenu {
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
        self.list.set_focus(true);
        self.list.render(f, area);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult {
        self.list.handle_key(key)
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
    }

    #[test]
    fn transitioning_machine_has_no_available_runtime_action() {
        let machine = MachineEntry::optimistic_nspawn("ubuntu", MachineState::Exiting);
        assert_eq!(
            ResourceActionMenu::for_machine(&machine).selected_action(),
            None
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
