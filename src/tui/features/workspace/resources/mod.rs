//! Resource browsing owns catalog selection and the corresponding list widgets.

pub(crate) mod actions;
pub(crate) mod catalog;
mod image_list;
mod list;
mod machine_list;
pub(crate) mod menu;
pub(crate) mod messages;
pub(crate) mod metrics;

use crate::domain::runtime::{ImageEntry, MachineEntry, MachineState};
use crate::tui::features::workspace::inspector::DetailTarget;
use crate::tui::features::workspace::resources::catalog::{ResourceCatalog, ResourceListKind};
use crate::tui::features::workspace::resources::image_list::ImageListComponent;
use crate::tui::features::workspace::resources::machine_list::MachineListComponent;
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::protocol::{AppMessage, EventResult, ListMessage};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};
use std::collections::HashSet;

pub(crate) struct ResourceBrowser {
    pub(crate) catalog: ResourceCatalog,
    machines: MachineListComponent,
    images: ImageListComponent,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ResourceInput {
    None,
    SelectionChanged(ResourceListKind),
    ImageTabChanged,
}

impl ResourceBrowser {
    pub(crate) fn new(systemd_tools: bool) -> Self {
        Self {
            catalog: ResourceCatalog::new(systemd_tools),
            machines: MachineListComponent::new(),
            images: ImageListComponent::new(),
        }
    }

    pub(crate) fn images_show_internal(&self) -> bool {
        self.images.shows_internal()
    }

    pub(crate) fn active_images(&self) -> (&[ImageEntry], usize) {
        self.catalog.active_images(self.images_show_internal())
    }

    pub(crate) fn selected_image(&self) -> Option<&ImageEntry> {
        let (images, selected) = self.active_images();
        images.get(selected)
    }

    pub(crate) fn selected_machine(&self) -> Option<&MachineEntry> {
        self.catalog.entries.get(self.catalog.selected)
    }

    pub(crate) fn machine(&self, name: &str) -> Option<&MachineEntry> {
        self.catalog.entries.iter().find(|entry| entry.name == name)
    }

    pub(crate) fn select_machine(&mut self, name: &str) {
        self.catalog.select_machine_by_name(name);
    }

    pub(crate) fn image(&self, name: &str) -> Option<&ImageEntry> {
        self.catalog
            .images
            .iter()
            .chain(&self.catalog.internal_images)
            .find(|image| image.name == name)
    }

    pub(crate) fn contains_target(&self, target: &DetailTarget) -> bool {
        match target {
            DetailTarget::Machine(name) => self.machine(name).is_some(),
            DetailTarget::Image { name, internal } => self
                .image(name)
                .is_some_and(|image| image.is_hidden() == *internal),
            DetailTarget::Empty => false,
        }
    }

    pub(crate) fn machine_target(&self) -> DetailTarget {
        self.selected_machine()
            .map(|entry| DetailTarget::Machine(entry.name.clone()))
            .unwrap_or_default()
    }

    pub(crate) fn image_target(&self) -> DetailTarget {
        self.selected_image()
            .map(|image| DetailTarget::Image {
                name: image.name.clone(),
                internal: image.is_hidden(),
            })
            .unwrap_or_default()
    }

    pub(crate) fn image_has_running_machine(&self, image: &ImageEntry) -> bool {
        !image.is_hidden()
            && self
                .machine(&image.name)
                .is_some_and(|entry| entry.state == MachineState::Running)
    }

    pub(crate) fn image_removable(&self, image: &ImageEntry, removing: &HashSet<String>) -> bool {
        !ImageEntry::is_protected_name(&image.name)
            && self.machine(&image.name).is_none()
            && !removing.contains(&image.name)
    }

    /// Both input sources commit selection here. The returned effect only
    /// asks the workspace to synchronize its inspector or terminal.
    pub(crate) fn handle_key(&mut self, list: ResourceListKind, key: KeyEvent) -> ResourceInput {
        let was_internal = self.images_show_internal();
        let result = match list {
            ResourceListKind::Machines => {
                self.machines
                    .handle_key(key, self.catalog.entries.len(), self.catalog.selected)
            }
            ResourceListKind::Images | ResourceListKind::InternalImages => {
                let (images, selected) = self.active_images();
                self.images.handle_key(key, images.len(), selected)
            }
        };
        self.finish_input(list, was_internal, result)
    }

    pub(crate) fn handle_mouse(
        &mut self,
        list: ResourceListKind,
        mouse: MouseEvent,
    ) -> ResourceInput {
        let was_internal = self.images_show_internal();
        let result = match list {
            ResourceListKind::Machines => self.machines.handle_mouse(mouse),
            ResourceListKind::Images | ResourceListKind::InternalImages => {
                self.images.handle_mouse(mouse)
            }
        };
        self.finish_input(list, was_internal, result)
    }

    fn finish_input(
        &mut self,
        list: ResourceListKind,
        was_internal: bool,
        result: EventResult,
    ) -> ResourceInput {
        if list != ResourceListKind::Machines && was_internal != self.images_show_internal() {
            return ResourceInput::ImageTabChanged;
        }
        let list = match list {
            ResourceListKind::Machines => ResourceListKind::Machines,
            _ if self.images_show_internal() => ResourceListKind::InternalImages,
            _ => ResourceListKind::Images,
        };
        match result {
            EventResult::Message(AppMessage::List(ListMessage::Next)) => {
                self.catalog.move_selection(list, true)
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.catalog.move_selection(list, false)
            }
            EventResult::Message(AppMessage::List(ListMessage::Select(index))) => {
                self.catalog.select_index(list, index)
            }
            _ => return ResourceInput::None,
        }
        ResourceInput::SelectionChanged(list)
    }

    pub(crate) fn render_machines(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        resize_mode: bool,
    ) {
        self.machines.render_with_data(
            frame,
            area,
            &self.catalog.entries,
            self.catalog.selected,
            focused,
            resize_mode,
        );
    }

    pub(crate) fn render_images(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        removing: &HashSet<String>,
        focused: bool,
        resize_mode: bool,
    ) {
        self.images.set_focus(focused);
        let (images, selected) = self.catalog.active_images(self.images_show_internal());
        self.images
            .render(frame, area, images, selected, removing, resize_mode);
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry, images: bool) {
        if images {
            self.images.register_footer_hints(footer);
        } else {
            self.machines.register_footer_hints(footer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn pointer_and_batched_keys_update_the_same_selection_before_the_next_frame() {
        let mut resources = ResourceBrowser::new(false);
        resources.catalog.replace_entries(
            (0..20)
                .map(|index| {
                    MachineEntry::optimistic_nspawn(
                        format!("machine-{index}"),
                        MachineState::Running,
                    )
                })
                .collect(),
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        terminal
            .draw(|frame| resources.render_machines(frame, frame.area(), true, false))
            .unwrap();

        let changed = ResourceInput::SelectionChanged(ResourceListKind::Machines);
        assert_eq!(
            resources.handle_mouse(
                ResourceListKind::Machines,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }
            ),
            changed
        );
        assert_eq!(resources.selected_machine().unwrap().name, "machine-1");
        assert_eq!(
            resources.handle_key(ResourceListKind::Machines, key(KeyCode::Up)),
            changed
        );
        assert_eq!(resources.catalog.selected, 0);
        resources.handle_key(ResourceListKind::Machines, key(KeyCode::PageDown));
        let first_page = resources.catalog.selected;
        assert!(first_page > 0);
        resources.handle_key(ResourceListKind::Machines, key(KeyCode::PageDown));
        assert!(resources.catalog.selected > first_page);
        resources.handle_key(ResourceListKind::Machines, key(KeyCode::Home));
        assert_eq!(resources.catalog.selected, 0);

        resources.catalog.replace_entries(Vec::new());
        resources.handle_key(ResourceListKind::Machines, key(KeyCode::Down));
        assert!(resources.selected_machine().is_none());
        assert_eq!(resources.catalog.selected, 0);
    }

    #[test]
    fn image_tabs_keep_independent_selection_when_the_catalog_is_replaced() {
        let image = |name: &str| ImageEntry {
            name: name.into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        };
        let mut resources = ResourceBrowser::new(false);
        resources
            .catalog
            .replace_images(vec![image("a"), image("b"), image(".a"), image(".b")]);
        resources.handle_key(ResourceListKind::Images, key(KeyCode::Down));
        assert_eq!(resources.selected_image().unwrap().name, "b");
        assert_eq!(
            resources.handle_key(ResourceListKind::Images, key(KeyCode::Char(']'))),
            ResourceInput::ImageTabChanged
        );
        resources.handle_key(ResourceListKind::Images, key(KeyCode::Down));
        assert_eq!(resources.selected_image().unwrap().name, ".b");

        resources.catalog.replace_images(vec![
            image("b"),
            image("a"),
            image("0"),
            image(".b"),
            image(".a"),
            image(".0"),
        ]);
        assert_eq!(resources.selected_image().unwrap().name, ".b");
        resources.handle_key(ResourceListKind::Images, key(KeyCode::Char('[')));
        assert_eq!(resources.selected_image().unwrap().name, "b");
    }
}
