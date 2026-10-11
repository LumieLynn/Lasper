//! Own the workspace and commit transitions between its panels.
//!
//! Resource selection, terminal selection and inspector targets change here
//! before control returns to App. Only commands which cross the workspace's
//! lifetime or feature boundary are returned to the application.

use crate::domain::runtime::{ImageEntry, MachineEntry, MachineState, RuntimeSnapshot};
use crate::tui::features::workspace::core::commands::WorkspaceInputResult;
use crate::tui::features::workspace::core::focus::WorkspaceNavigation;
use crate::tui::features::workspace::core::input::WorkspaceEvent;
use crate::tui::features::workspace::core::layout::WorkspaceLayout;
use crate::tui::features::workspace::inspector::refresh::{
    DetailRefreshCompletion, DetailRefreshServices,
};
use crate::tui::features::workspace::inspector::state::DetailRefreshEffects;
use crate::tui::features::workspace::inspector::{DetailTarget, Inspector};
use crate::tui::features::workspace::resources::catalog::ResourceListKind;
use crate::tui::features::workspace::resources::ResourceBrowser;
use crate::tui::features::workspace::terminal_panel::TerminalManager;
use crate::tui::features::workspace::WorkspaceFocus;
use crate::tui::foundation::input::UiInput;

pub(crate) struct Workspace {
    pub(super) navigation: WorkspaceNavigation,
    pub(super) layout: WorkspaceLayout,
    pub(crate) resources: ResourceBrowser,
    pub(crate) inspector: Inspector,
}

impl Workspace {
    pub(crate) fn new(systemd_tools: bool, log_buffer_lines: usize) -> Self {
        Self {
            navigation: WorkspaceNavigation::default(),
            layout: WorkspaceLayout::default(),
            resources: ResourceBrowser::new(systemd_tools),
            inspector: Inspector::new(log_buffer_lines),
        }
    }

    pub(crate) fn focus(&self) -> WorkspaceFocus {
        self.navigation.current()
    }

    /// External workflows enter a panel through the same target transition
    /// used by local keyboard and pointer navigation.
    pub(crate) fn set_focus(
        &mut self,
        focus: WorkspaceFocus,
        terminal_machine: Option<&str>,
    ) -> bool {
        let changed = self.navigation.set_focus(focus);
        self.synchronize_inspector(terminal_machine);
        changed
    }

    pub(crate) fn handle_input(
        &mut self,
        input: UiInput,
        terminal: &mut TerminalManager,
    ) -> WorkspaceInputResult {
        let previous_focus = self.focus();
        let input = self.route_input(input, terminal);
        let mut refresh = input.focus.is_some_and(|focus| focus != previous_focus);
        if let Some(focus) = input.focus {
            self.navigation.set_focus(focus);
        }
        let command = match input.event {
            WorkspaceEvent::None => None,
            WorkspaceEvent::Command(command) => Some(command),
            WorkspaceEvent::ResourceSelectionChanged(list) => {
                if list == ResourceListKind::Machines {
                    self.sync_terminal_selection(terminal);
                }
                refresh = true;
                None
            }
            WorkspaceEvent::ImageTabChanged | WorkspaceEvent::InspectorPaneChanged => {
                refresh = true;
                None
            }
            WorkspaceEvent::TerminalSessionChanged(machine) => {
                self.resources.select_machine(&machine);
                refresh = true;
                None
            }
        };
        if refresh {
            let machine = terminal
                .active_session()
                .map(|session| session.machine_name.as_str());
            self.request_inspection(machine);
        } else if input.focus.is_some() {
            let machine = terminal
                .active_session()
                .map(|session| session.machine_name.as_str());
            self.synchronize_inspector(machine);
        }
        WorkspaceInputResult {
            command,
            focus_changed: self.focus() != previous_focus,
        }
    }

    fn sync_terminal_selection(&mut self, terminal: &mut TerminalManager) {
        let Some(machine) = self.resources.selected_machine() else {
            return;
        };
        terminal.sync_to_entry(&machine.name);
        if self.focus().is_terminal() && !terminal.is_showing() {
            self.navigation
                .set_focus(self.navigation.non_terminal_destination());
        }
    }

    pub(crate) fn focused_image(&self) -> Option<&ImageEntry> {
        match self.focus() {
            WorkspaceFocus::Images => self.resources.selected_image(),
            WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector => {
                let DetailTarget::Image { name, .. } = self.inspector.target() else {
                    return None;
                };
                self.resources.image(name)
            }
            _ => None,
        }
    }

    pub(crate) fn focused_machine(&self) -> Option<&MachineEntry> {
        match self.focus() {
            WorkspaceFocus::Machines => self.resources.selected_machine(),
            WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector => {
                let DetailTarget::Machine(name) = self.inspector.target() else {
                    return None;
                };
                self.resources.machine(name)
            }
            _ => None,
        }
    }

    /// Launching from an image selects its running machine before the
    /// application opens the session. Internal images cannot provide one.
    pub(crate) fn terminal_target(&mut self) -> Result<Option<MachineEntry>, String> {
        let Some(image) = self.focused_image() else {
            return Ok(self.resources.selected_machine().cloned());
        };
        if image.is_hidden() {
            return Err("Internal images do not provide terminal sessions.".into());
        }
        let Some(machine) = self.resources.machine(&image.name).cloned() else {
            return Err(format!("{} is not running.", image.name));
        };
        if machine.state != MachineState::Running {
            return Err(format!(
                "{} is {} and cannot accept a terminal.",
                image.name,
                machine.state.label(),
            ));
        }
        self.resources.select_machine(&machine.name);
        Ok(Some(machine))
    }

    fn synchronize_inspector(&mut self, terminal_machine: Option<&str>) {
        let target = match self.focus() {
            WorkspaceFocus::Machines => self.resources.machine_target(),
            WorkspaceFocus::Images => self.resources.image_target(),
            WorkspaceFocus::MachineInspector
                if matches!(self.inspector.target(), DetailTarget::Machine(_))
                    && self.resources.contains_target(self.inspector.target()) =>
            {
                self.inspector.target().clone()
            }
            WorkspaceFocus::ImageInspector
                if matches!(self.inspector.target(), DetailTarget::Image { .. })
                    && self.resources.contains_target(self.inspector.target()) =>
            {
                self.inspector.target().clone()
            }
            WorkspaceFocus::MachineInspector => self.resources.machine_target(),
            WorkspaceFocus::ImageInspector => self.resources.image_target(),
            WorkspaceFocus::Terminal => terminal_machine
                .and_then(|name| self.resources.machine(name))
                .map(|machine| DetailTarget::Machine(machine.name.clone()))
                .unwrap_or_else(|| self.resources.machine_target()),
        };
        self.inspector.set_target(target);
    }

    /// Callers request inspection of the workspace's current context, rather
    /// than reconstructing the relationship between focus and resource lists.
    pub(crate) fn request_inspection(&mut self, terminal_machine: Option<&str>) {
        self.synchronize_inspector(terminal_machine);
        self.inspector.request_refresh();
    }

    pub(crate) fn start_inspection(
        &mut self,
        services: DetailRefreshServices,
        tx: &tokio::sync::mpsc::Sender<DetailRefreshCompletion>,
    ) -> DetailRefreshEffects {
        self.inspector.drain_logs();
        self.inspector
            .start_refresh(&self.resources.catalog.entries, services, tx)
    }

    pub(crate) fn replace_machines(&mut self, entries: Vec<MachineEntry>) {
        if self.resources.catalog.replace_entries(entries) {
            self.inspector.invalidate_overview();
        }
        self.resources.catalog.retain_metrics_for_entries();
        self.inspector
            .retain_machine_logs(&self.resources.catalog.entries);
    }

    pub(crate) fn replace_images(&mut self, images: Vec<ImageEntry>) {
        if self.resources.catalog.replace_images(images) {
            self.inspector.invalidate_overview();
        }
    }

    /// A runtime update commits both catalogs before choosing an inspector
    /// target, so panels never observe a partially replaced snapshot.
    pub(crate) fn replace_snapshot(
        &mut self,
        snapshot: RuntimeSnapshot,
        terminal_machine: Option<&str>,
    ) {
        self.replace_machines(snapshot.machines);
        self.replace_images(snapshot.images);
        self.request_inspection(terminal_machine);
    }

    #[cfg(test)]
    pub(crate) async fn inspect_now(
        &mut self,
        terminal_machine: Option<&str>,
        services: DetailRefreshServices,
    ) -> DetailRefreshEffects {
        self.synchronize_inspector(terminal_machine);
        self.inspector
            .refresh_now(&self.resources.catalog.entries, services)
            .await
    }

    pub(crate) fn restore_non_terminal_focus(&self) -> WorkspaceFocus {
        self.navigation.non_terminal_destination()
    }

    /// Terminal startup uses the most recently rendered right-pane height,
    /// with the minimum needed for a usable shell.
    pub(crate) fn terminal_rows(&self) -> u16 {
        self.layout.terminal_rows()
    }

    #[cfg(test)]
    pub(crate) fn panel_areas(
        &self,
    ) -> crate::tui::features::workspace::core::layout::WorkspaceAreas {
        self.layout.areas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspector_focus_keeps_only_the_matching_kind_of_target() {
        use crate::domain::runtime::MachineState;
        let mut workspace = Workspace::new(false, 5000);
        workspace.replace_machines(vec![MachineEntry::optimistic_nspawn(
            "machine",
            MachineState::Running,
        )]);
        let image = ImageEntry {
            name: "image".into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        };
        workspace.replace_images(vec![image]);

        workspace.inspector.set_target(DetailTarget::Image {
            name: "image".into(),
            internal: false,
        });
        workspace.set_focus(WorkspaceFocus::MachineInspector, None);
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Machine("machine".into())
        );

        workspace.set_focus(WorkspaceFocus::ImageInspector, None);
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Image {
                name: "image".into(),
                internal: false
            }
        );
    }

    #[test]
    fn removed_inspector_target_falls_back_to_the_current_resource_selection() {
        use crate::domain::runtime::MachineState;
        let mut workspace = Workspace::new(false, 5000);
        workspace.replace_machines(vec![
            MachineEntry::optimistic_nspawn("selected", MachineState::Running),
            MachineEntry::optimistic_nspawn("retained", MachineState::Running),
        ]);
        workspace
            .inspector
            .set_target(DetailTarget::Machine("retained".into()));
        workspace.set_focus(WorkspaceFocus::MachineInspector, None);
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Machine("retained".into())
        );

        workspace.replace_machines(vec![MachineEntry::optimistic_nspawn(
            "selected",
            MachineState::Running,
        )]);
        workspace.synchronize_inspector(None);
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Machine("selected".into())
        );
        workspace.replace_machines(Vec::new());
        workspace.synchronize_inspector(None);
        assert_eq!(workspace.inspector.target(), &DetailTarget::Empty);
    }
}
