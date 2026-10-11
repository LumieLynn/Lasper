//! Route input to workspace panels and describe local transitions.
//!
//! Panels handle their own controls. The workspace manager commits the
//! resulting cross-panel transitions before returning application commands.

use crate::tui::features::workspace::core::commands::WorkspaceCommand;
use crate::tui::features::workspace::core::focus::WorkspaceNavigation;
use crate::tui::features::workspace::core::layout::{ResizeDirection, WorkspacePanel};
use crate::tui::features::workspace::core::shortcuts::WorkspaceShortcut;
use crate::tui::features::workspace::inspector::DetailTarget;
use crate::tui::features::workspace::resources::catalog::ResourceListKind;
use crate::tui::features::workspace::terminal_panel::{
    TerminalInputStatus, TerminalKeyOutcome, TerminalManager,
};
use crate::tui::features::workspace::{Workspace, WorkspaceFocus};
use crate::tui::foundation::input::UiInput;
use crate::tui::protocol::{AppMessage, ContainerMessage, EventResult};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};

#[derive(Debug, PartialEq)]
pub(super) enum WorkspaceEvent {
    None,
    Command(WorkspaceCommand),
    ResourceSelectionChanged(ResourceListKind),
    ImageTabChanged,
    InspectorPaneChanged,
    TerminalSessionChanged(String),
}

#[derive(Debug, PartialEq)]
pub(super) struct WorkspaceInput {
    pub focus: Option<WorkspaceFocus>,
    pub event: WorkspaceEvent,
}

impl WorkspaceInput {
    fn event(event: WorkspaceEvent) -> Self {
        Self { focus: None, event }
    }

    fn focused(mut self, focus: Option<WorkspaceFocus>) -> Self {
        if self.focus.is_none() {
            self.focus = focus;
        }
        self
    }
}

impl Workspace {
    /// Modal layers have already received first refusal. Resize mode and a
    /// focused terminal intercept keys before workspace shortcuts. Pointer
    /// capture precedes hit-testing, and each widget owns its internal geometry.
    pub(super) fn route_input(
        &mut self,
        input: UiInput,
        terminal: &mut TerminalManager,
    ) -> WorkspaceInput {
        match input {
            UiInput::Key(key) => self.handle_key_input(key, terminal),
            UiInput::Mouse(mouse) => self.handle_mouse_input(mouse, terminal),
        }
    }

    fn handle_key_input(
        &mut self,
        key: KeyEvent,
        terminal: &mut TerminalManager,
    ) -> WorkspaceInput {
        use WorkspaceEvent as Event;

        let shortcut = WorkspaceShortcut::from_key(key);
        if !self.focus().is_terminal() && shortcut == Some(WorkspaceShortcut::Leader) {
            return WorkspaceInput::event(Event::Command(WorkspaceCommand::Leader));
        }

        let terminal_insert = self.focus().is_terminal() && terminal.in_insert_mode();
        if self.layout.is_resizing()
            && !terminal_insert
            && self.handle_resize_key(key.code, terminal.is_showing())
        {
            return WorkspaceInput::event(Event::None);
        }

        let restored_focus = if self.focus().is_terminal() {
            let outcome = terminal.handle_key(key);
            let focus = (!terminal.is_showing()).then(|| self.restore_non_terminal_focus());
            let event = match outcome {
                TerminalKeyOutcome::PassThrough => None,
                TerminalKeyOutcome::Consumed => Some(Event::None),
                TerminalKeyOutcome::SessionChanged(machine) => {
                    Some(Event::TerminalSessionChanged(machine))
                }
                TerminalKeyOutcome::InputQueueFull => Some(Event::Command(
                    WorkspaceCommand::TerminalInputRejected(TerminalInputStatus::Full),
                )),
                TerminalKeyOutcome::InputChannelClosed => Some(Event::Command(
                    WorkspaceCommand::TerminalInputRejected(TerminalInputStatus::Closed),
                )),
            };
            if let Some(event) = event {
                return WorkspaceInput::event(event).focused(focus);
            }
            focus
        } else {
            None
        };

        let effective_focus = restored_focus.unwrap_or(self.focus());
        if let Some(result) = self.workspace_shortcut(shortcut, effective_focus, terminal) {
            return result.focused(restored_focus);
        }

        let event = match effective_focus {
            WorkspaceFocus::Machines => {
                resource_event(self.resources.handle_key(ResourceListKind::Machines, key))
            }
            WorkspaceFocus::Images => {
                resource_event(self.resources.handle_key(ResourceListKind::Images, key))
            }
            WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector => {
                inspector_event(self.inspector.handle_key(key))
            }
            WorkspaceFocus::Terminal => Event::None,
        };
        WorkspaceInput::event(event).focused(restored_focus)
    }

    fn workspace_shortcut(
        &mut self,
        shortcut: Option<WorkspaceShortcut>,
        focus: WorkspaceFocus,
        terminal: &mut TerminalManager,
    ) -> Option<WorkspaceInput> {
        use WorkspaceCommand as Command;
        use WorkspaceEvent as Event;
        use WorkspaceShortcut as Shortcut;
        let shortcut = shortcut?;
        let event = match shortcut {
            Shortcut::Quit => Event::Command(Command::Quit),
            Shortcut::Help => Event::Command(Command::Help),
            Shortcut::NextPanel | Shortcut::PreviousPanel => {
                return Some(WorkspaceInput {
                    focus: Some(WorkspaceNavigation::next_focus(
                        focus,
                        shortcut == Shortcut::NextPanel,
                        terminal.is_showing(),
                        terminal.maximized,
                    )),
                    event: Event::None,
                });
            }
            Shortcut::Start => Event::Command(Command::Start),
            Shortcut::Poweroff => Event::Command(Command::Poweroff),
            Shortcut::ResourceActions => Event::Command(Command::ResourceActions),
            Shortcut::Wizard => Event::Command(Command::Wizard),
            Shortcut::Refresh => Event::Command(Command::Refresh),
            Shortcut::Resize => {
                self.layout.toggle_resize();
                Event::None
            }
            Shortcut::Shell => Event::Command(Command::Shell),
            Shortcut::ToggleTerminal => {
                if terminal.is_showing() {
                    terminal.maximized = !terminal.maximized;
                    if terminal.maximized && focus.is_inspector() {
                        return Some(WorkspaceInput {
                            focus: Some(WorkspaceFocus::Terminal),
                            event: Event::None,
                        });
                    }
                }
                Event::None
            }
            Shortcut::RemoveImage => Event::Command(Command::RemoveImage),
            Shortcut::Leader => return None,
        };
        Some(WorkspaceInput::event(event))
    }

    fn handle_mouse_input(
        &mut self,
        mouse: MouseEvent,
        terminal: &mut TerminalManager,
    ) -> WorkspaceInput {
        // Finish a drag even if the pointer has crossed another panel. A
        // widget underneath must not steal the matching release event.
        if terminal.is_showing()
            && terminal.wants_mouse_capture()
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(_) | MouseEventKind::Up(_) | MouseEventKind::Moved
            )
        {
            return WorkspaceInput::event(terminal_mouse_event(terminal.handle_mouse(mouse)));
        }

        let hit = self.focus_at(mouse);
        let focus = match (hit, mouse.kind) {
            (Some(focus), MouseEventKind::Down(_)) => Some(focus),
            (Some(focus), MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
                if matches!(focus, WorkspaceFocus::Machines | WorkspaceFocus::Images) =>
            {
                Some(focus)
            }
            _ => None,
        };
        let effective_focus = focus.unwrap_or(self.focus());

        let event = match hit {
            Some(WorkspaceFocus::Machines) => resource_event(
                self.resources
                    .handle_mouse(ResourceListKind::Machines, mouse),
            ),
            Some(WorkspaceFocus::Images) => {
                resource_event(self.resources.handle_mouse(ResourceListKind::Images, mouse))
            }
            Some(WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector) => {
                inspector_event(self.inspector.handle_mouse(mouse))
            }
            Some(WorkspaceFocus::Terminal) => {
                if let Some(machine) = terminal.handle_tab_click(mouse) {
                    WorkspaceEvent::TerminalSessionChanged(machine)
                } else if effective_focus.is_terminal() && terminal.is_showing() {
                    terminal_mouse_event(terminal.handle_mouse(mouse))
                } else {
                    WorkspaceEvent::None
                }
            }
            None => WorkspaceEvent::None,
        };
        WorkspaceInput::event(event).focused(focus)
    }

    fn focus_at(&self, mouse: MouseEvent) -> Option<WorkspaceFocus> {
        Some(match self.layout.panel_at(mouse.column, mouse.row)? {
            WorkspacePanel::Machines => WorkspaceFocus::Machines,
            WorkspacePanel::Images => WorkspaceFocus::Images,
            WorkspacePanel::Terminal => WorkspaceFocus::Terminal,
            WorkspacePanel::Inspector => match self.inspector.target() {
                DetailTarget::Image { .. } => WorkspaceFocus::ImageInspector,
                DetailTarget::Machine(_) => WorkspaceFocus::MachineInspector,
                DetailTarget::Empty
                    if matches!(
                        self.focus(),
                        WorkspaceFocus::Images | WorkspaceFocus::ImageInspector
                    ) =>
                {
                    WorkspaceFocus::ImageInspector
                }
                DetailTarget::Empty => WorkspaceFocus::MachineInspector,
            },
        })
    }

    fn handle_resize_key(&mut self, key: KeyCode, terminal_showing: bool) -> bool {
        let direction = match key {
            KeyCode::Esc | KeyCode::Char('R') | KeyCode::Char('q') => {
                self.layout.stop_resize();
                return true;
            }
            KeyCode::Char('h') | KeyCode::Left => ResizeDirection::Left,
            KeyCode::Char('l') | KeyCode::Right => ResizeDirection::Right,
            KeyCode::Char('j') | KeyCode::Down => ResizeDirection::Down,
            KeyCode::Char('k') | KeyCode::Up => ResizeDirection::Up,
            KeyCode::Tab | KeyCode::BackTab => return false,
            _ => return true,
        };
        self.layout
            .resize(direction, self.focus(), terminal_showing);
        true
    }
}

fn resource_event(
    result: crate::tui::features::workspace::resources::ResourceInput,
) -> WorkspaceEvent {
    use crate::tui::features::workspace::resources::ResourceInput;
    match result {
        ResourceInput::None => WorkspaceEvent::None,
        ResourceInput::SelectionChanged(list) => WorkspaceEvent::ResourceSelectionChanged(list),
        ResourceInput::ImageTabChanged => WorkspaceEvent::ImageTabChanged,
    }
}

fn inspector_event(result: EventResult) -> WorkspaceEvent {
    match result {
        EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(_))) => {
            WorkspaceEvent::InspectorPaneChanged
        }
        _ => WorkspaceEvent::None,
    }
}

fn terminal_mouse_event(status: TerminalInputStatus) -> WorkspaceEvent {
    match status {
        TerminalInputStatus::Queued => WorkspaceEvent::None,
        TerminalInputStatus::Full | TerminalInputStatus::Closed => {
            WorkspaceEvent::Command(WorkspaceCommand::TerminalInputRejected(status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::NspawnConfigStore;
    use crate::adapters::session::{
        DirectSessionAdapter, DirectTerminalPolicy, MachineSessionTransport,
    };
    use crate::application::sessions::SessionService;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton};
    use ratatui::layout::Rect;
    use std::sync::Arc;

    fn terminal_manager() -> TerminalManager {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            MachineSessionTransport::SystemdTools,
            NspawnConfigStore::direct(),
        ))));
        TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES)
    }

    #[test]
    fn image_tab_input_commits_target_and_refresh_without_an_app_command() {
        let mut workspace = Workspace::new(false, 5000);
        workspace.replace_images(vec![crate::domain::runtime::ImageEntry {
            name: ".internal".into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        }]);
        workspace.set_focus(WorkspaceFocus::Images, None);
        let mut terminal = terminal_manager();

        let result = workspace.handle_input(
            UiInput::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            &mut terminal,
        );

        assert!(!result.focus_changed);
        assert_eq!(result.command, None);
        assert!(workspace.resources.images_show_internal());
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Image {
                name: ".internal".into(),
                internal: true,
            }
        );
        assert!(workspace.inspector.state.refresh.has_pending());
    }

    #[test]
    fn focus_cycle_uses_the_restored_destination_when_terminal_is_hidden() {
        let mut workspace = Workspace::new(false, 5000);
        workspace.set_focus(WorkspaceFocus::Images, None);
        workspace.set_focus(WorkspaceFocus::Terminal, None);
        let mut terminal = terminal_manager();

        let result = workspace.handle_input(
            UiInput::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut terminal,
        );

        assert!(result.focus_changed);
        assert_eq!(workspace.focus(), WorkspaceFocus::ImageInspector);
    }

    #[test]
    fn clicking_the_inspector_focuses_it_while_list_wheels_are_scoped() {
        let mut workspace = Workspace::new(false, 5000);
        workspace.layout.areas.detail = Some(Rect::new(10, 0, 20, 10));
        workspace
            .inspector
            .set_target(DetailTarget::Machine("machine".into()));
        let mut terminal = terminal_manager();

        let result = workspace.handle_input(
            UiInput::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            &mut terminal,
        );

        assert!(result.focus_changed);
        assert_eq!(workspace.focus(), WorkspaceFocus::MachineInspector);
    }

    async fn open_prompt(terminal: &mut TerminalManager) {
        let entry = crate::domain::runtime::MachineEntry {
            name: "machine".into(),
            class: crate::domain::runtime::MachineEntry::NSPAWN_CLASS.into(),
            service: crate::domain::runtime::MachineEntry::NSPAWN_SERVICE.into(),
            state: crate::domain::runtime::MachineState::Running,
            addresses: Default::default(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        terminal
            .spawn_shell_prompt(&entry, 10, &Some(tx))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn machine_selection_commits_terminal_and_inspector_before_returning() {
        use crate::domain::runtime::{MachineEntry, MachineState};
        let mut workspace = Workspace::new(false, 5000);
        workspace.replace_machines(vec![
            MachineEntry::optimistic_nspawn("machine", MachineState::Running),
            MachineEntry::optimistic_nspawn("other", MachineState::Running),
        ]);
        workspace.set_focus(WorkspaceFocus::Machines, None);
        let mut terminal = terminal_manager();
        open_prompt(&mut terminal).await;
        assert!(terminal.is_showing());

        let result = workspace.handle_input(
            UiInput::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &mut terminal,
        );

        assert_eq!(result.command, None);
        assert!(!result.focus_changed);
        assert_eq!(
            workspace.resources.selected_machine().unwrap().name,
            "other"
        );
        assert!(!terminal.is_showing());
        assert_eq!(
            workspace.inspector.target(),
            &DetailTarget::Machine("other".into())
        );
        assert!(workspace.inspector.state.refresh.has_pending());
        terminal.cleanup_all();
    }

    #[tokio::test]
    async fn terminal_insert_input_precedes_workspace_shortcuts_and_resize() {
        let mut workspace = Workspace::new(false, 5000);
        workspace.set_focus(WorkspaceFocus::Terminal, None);
        workspace.layout.toggle_resize();
        workspace
            .inspector
            .set_target(DetailTarget::Machine("machine".into()));
        let mut terminal = terminal_manager();
        open_prompt(&mut terminal).await;

        for character in ['q', 'R', ' '] {
            let result = workspace.handle_input(
                UiInput::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                &mut terminal,
            );
            assert_eq!(result.command, None);
            assert!(!result.focus_changed);
            assert!(workspace.layout.is_resizing());
        }
        terminal.cleanup_all();
    }

    #[tokio::test]
    async fn terminal_pointer_capture_finishes_after_focus_moves_to_another_panel() {
        let mut workspace = Workspace::new(false, 5000);
        workspace.set_focus(WorkspaceFocus::Terminal, None);
        workspace.layout.areas.terminal = Some(Rect::new(10, 0, 20, 10));
        workspace.layout.areas.images = Rect::new(0, 0, 10, 10);
        workspace
            .inspector
            .set_target(DetailTarget::Machine("machine".into()));
        let mut terminal = terminal_manager();
        open_prompt(&mut terminal).await;
        terminal.term_area = Rect::new(11, 1, 18, 8);

        workspace.handle_input(
            UiInput::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 11,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut terminal,
        );
        assert!(terminal.wants_mouse_capture());

        workspace.set_focus(WorkspaceFocus::Images, None);
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let result = workspace.handle_input(
                UiInput::Mouse(MouseEvent {
                    kind,
                    column: 2,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                }),
                &mut terminal,
            );
            assert_eq!(result.command, None);
            assert!(!result.focus_changed);
        }
        assert!(!terminal.wants_mouse_capture());
        assert!(!terminal.active_session().unwrap().selection.active);
        terminal.cleanup_all();
    }

    #[test]
    fn maximized_terminal_routes_pointer_hits_to_the_visible_terminal() {
        let mut workspace = Workspace::new(false, 5000);
        let layout = workspace.layout.update(Rect::new(0, 0, 40, 10), true, true);
        let terminal = layout.terminal.unwrap();
        workspace
            .inspector
            .set_target(DetailTarget::Machine("machine".into()));

        assert_eq!(
            workspace.focus_at(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: terminal.x + 1,
                row: terminal.y + 1,
                modifiers: KeyModifiers::NONE,
            },),
            Some(WorkspaceFocus::Terminal)
        );
    }
}
