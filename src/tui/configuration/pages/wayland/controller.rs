//! Workspace contract and input routing for the Wayland page.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::Frame;

use super::runtime::WaylandRuntimeRequest;
use super::{presentation, WaylandDraftItem, WaylandPageAction, WaylandPageState};
use crate::application::configuration::{
    ConfigurationDraftRequest, ConfigurationSnapshot, ConfigurationTarget,
};
use crate::application::sessions::{SessionError, WaylandSessionContext};
use crate::tui::configuration::core::page::{
    ConfigurationPageController, ConfigurationPageDescriptor, ConfigurationSectionId, PageInput,
    PageInspectionReport, PageInteractionContext, PageRenderContext, PageRequest,
};
use crate::tui::configuration::pages::ConfigurationPageAction;

pub(crate) enum WaylandPageUpdate {
    TrackCheck(tokio::task::JoinHandle<()>),
    Checked {
        generation: u64,
        result: Result<WaylandSessionContext, SessionError>,
    },
}

impl WaylandPageState {
    fn draft_request(&self, snapshot: &ConfigurationSnapshot) -> Option<ConfigurationDraftRequest> {
        match self.selected_item(snapshot)? {
            WaylandDraftItem::Declaration(line) => {
                Some(ConfigurationDraftRequest::ToggleWaylandDeclaration(line))
            }
            WaylandDraftItem::Available {
                source,
                guest_target,
            } => Some(ConfigurationDraftRequest::ToggleWaylandSource {
                source,
                guest_target,
            }),
        }
    }

    fn handle_page_key(
        &mut self,
        key: KeyEvent,
        context: PageInteractionContext<'_>,
    ) -> PageRequest {
        if let Some(request) = self.handle_access_key(key) {
            return match request {
                WaylandRuntimeRequest::None => PageRequest::None,
                WaylandRuntimeRequest::Action(action @ WaylandPageAction::EnterShell { .. }) => {
                    PageRequest::CleanDraftAction {
                        action: ConfigurationPageAction::Wayland(action),
                        blocked_message:
                            "Save or discard pending configuration changes before entering a shell"
                                .into(),
                    }
                }
                WaylandRuntimeRequest::Action(action) => {
                    PageRequest::Action(ConfigurationPageAction::Wayland(action))
                }
            };
        }
        if key.modifiers != KeyModifiers::NONE {
            return PageRequest::None;
        }
        let runtime_available = matches!(context.target, ConfigurationTarget::Machine(_));
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_focus(true, runtime_available),
            KeyCode::Up | KeyCode::Char('k') => self.move_focus(false, runtime_available),
            KeyCode::Left if self.is_bindings_focused() => {
                self.checklist.set_selected_expanded(false);
            }
            KeyCode::Right if self.is_bindings_focused() => {
                self.checklist.set_selected_expanded(true);
            }
            KeyCode::Char(' ') if self.is_bindings_focused() => {
                if let Some(request) = context
                    .snapshot
                    .and_then(|snapshot| self.draft_request(snapshot))
                {
                    return PageRequest::Draft(request);
                }
            }
            KeyCode::Enter if self.is_bindings_focused() => self.toggle_selected_details(),
            KeyCode::Enter | KeyCode::Char('c') if runtime_available => {
                if let Some(snapshot) = context.snapshot {
                    self.open_access_dialog(context.target, snapshot);
                }
            }
            _ => {}
        }
        PageRequest::None
    }

    fn handle_page_click(
        &mut self,
        position: Position,
        context: PageInteractionContext<'_>,
    ) -> PageRequest {
        let Some(snapshot) = context.snapshot else {
            return PageRequest::None;
        };
        if self.checklist.hits.access.contains(position)
            && matches!(context.target, ConfigurationTarget::Machine(_))
        {
            self.set_runtime_access_focus();
            self.open_access_dialog(context.target, snapshot);
            return PageRequest::None;
        }
        self.set_bindings_focus();
        let toggle = self
            .checklist
            .hits
            .checkboxes
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, index)| *index);
        let selected = self
            .checklist
            .hits
            .bindings
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, index)| *index);
        if let Some(selected) = selected {
            self.select_item(selected);
            if toggle == Some(selected) {
                if let Some(request) = self.draft_request(snapshot) {
                    return PageRequest::Draft(request);
                }
            }
        }
        PageRequest::None
    }
}

impl ConfigurationPageController for WaylandPageState {
    type Update = WaylandPageUpdate;

    fn descriptor(&self) -> ConfigurationPageDescriptor {
        ConfigurationPageDescriptor {
            id: super::PAGE_ID,
            section: ConfigurationSectionId::HostIntegration,
            label: "Wayland",
        }
    }

    fn reset(&mut self) {
        self.begin_query();
    }

    fn load(&mut self, snapshot: &ConfigurationSnapshot) {
        self.finish_query(snapshot);
    }

    fn inspection_report(&self, snapshot: &ConfigurationSnapshot) -> PageInspectionReport {
        presentation::inspection_report(snapshot)
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, context: PageRenderContext<'_>) {
        self.checklist.hits = presentation::render(
            self,
            frame,
            area,
            context.state,
            context.target,
            context.pane,
            context.draft,
        );
    }

    fn render_overlay(&mut self, frame: &mut Frame, area: Rect) {
        self.render_access_dialog(frame, area);
    }

    fn modal_open(&self) -> bool {
        self.access_dialog_is_open()
    }

    fn reject_action(&mut self, message: String) {
        self.block_shell(message);
    }

    fn handle_input(
        &mut self,
        input: PageInput,
        context: PageInteractionContext<'_>,
    ) -> PageRequest {
        match input {
            PageInput::Key(key) => self.handle_page_key(key, context),
            PageInput::Click(position) => self.handle_page_click(position, context),
            PageInput::Scroll(down) => {
                self.move_focus(
                    down,
                    matches!(context.target, ConfigurationTarget::Machine(_)),
                );
                PageRequest::None
            }
        }
    }

    fn update(&mut self, update: Self::Update) {
        match update {
            WaylandPageUpdate::TrackCheck(task) => self.track_check(task),
            WaylandPageUpdate::Checked { generation, result } => {
                if let Some(dialog) = &mut self.access_dialog {
                    dialog.finish_check(generation, result);
                }
            }
        }
    }

    #[cfg(test)]
    fn test_state(&self) -> crate::tui::configuration::core::page::PageTestState {
        crate::tui::configuration::core::page::PageTestState {
            selected: self.checklist.selected(),
            expanded: self.checklist.expanded.clone(),
            modal_open: self.access_dialog_is_open(),
            bindings: self.checklist.hits.bindings.clone(),
            checkboxes: self.checklist.hits.checkboxes.clone(),
            access: self.checklist.hits.access,
        }
    }
}
