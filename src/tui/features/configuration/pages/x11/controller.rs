//! Workspace contract and input routing for the X11 page.

use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::selection::register_selection_hints;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::Frame;

use super::request::{X11PageAction, X11PageUpdate};
use super::{item_toggle_enabled, presentation, X11DraftItem, X11PageState, PAGE_ID};
use crate::application::configuration::{
    ConfigurationDraftRequest, ConfigurationSnapshot, ConfigurationTarget,
};
use crate::tui::features::configuration::page::{ConfigurationPageController, PageRequest};
use crate::tui::features::configuration::page::{
    ConfigurationPageDescriptor, PageInput, PageInspectionReport, PageInteractionContext,
    PageRenderContext,
};

impl X11PageState {
    fn draft_request(
        &self,
        snapshot: &ConfigurationSnapshot,
        draft: &crate::application::configuration::ConfigurationDraft,
    ) -> Option<ConfigurationDraftRequest> {
        let index = self.checklist.selected()?;
        let item = self.checklist.items.get(index)?;
        if !item_toggle_enabled(item, snapshot, draft) {
            return None;
        }
        match self.selected_item()? {
            X11DraftItem::Declaration(line) => {
                Some(ConfigurationDraftRequest::ToggleX11Declaration(line))
            }
            X11DraftItem::Directory(source) | X11DraftItem::Available(source) => {
                Some(ConfigurationDraftRequest::ToggleX11Source(source))
            }
        }
    }

    fn handle_page_key(
        &mut self,
        key: KeyEvent,
        context: PageInteractionContext<'_>,
    ) -> PageRequest<X11PageAction> {
        if let Some(request) = self.handle_access_key(key) {
            return request.into();
        }
        if key.modifiers != KeyModifiers::NONE {
            return PageRequest::None;
        }
        let runtime_available = matches!(context.target, ConfigurationTarget::Machine(_));
        if let Some(action) = ScrollAction::from_key(key) {
            self.move_focus(action, runtime_available);
            return PageRequest::None;
        }
        match key.code {
            KeyCode::Left if self.is_bindings_focused() => {
                self.checklist.set_selected_expanded(false);
            }
            KeyCode::Right if self.is_bindings_focused() => {
                self.checklist.set_selected_expanded(true);
            }
            KeyCode::Char(' ') if self.is_bindings_focused() => {
                if let Some(snapshot) = context.snapshot {
                    if let Some(request) = self.draft_request(snapshot, context.draft) {
                        return PageRequest::Draft(request);
                    }
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
    ) -> PageRequest<X11PageAction> {
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
                if let Some(request) = self.draft_request(snapshot, context.draft) {
                    return PageRequest::Draft(request);
                }
            }
        }
        PageRequest::None
    }
}

impl ConfigurationPageController for X11PageState {
    fn register_footer_hints(
        &self,
        footer: &mut FooterRegistry,
        context: PageInteractionContext<'_>,
    ) {
        if let Some(dialog) = &self.access_dialog {
            dialog.register_footer_hints(footer);
            return;
        }
        let Some(snapshot) = context.snapshot else {
            return;
        };
        register_selection_hints(footer);
        if self.is_bindings_focused() {
            if self.draft_request(snapshot, context.draft).is_some() {
                footer.register(FooterHint::new("Space", "Toggle").on_key(KeyCode::Char(' ')));
            }
            if self.checklist.selected().is_some() {
                footer.register(FooterHint::new("Enter", "Fold").on_key(KeyCode::Enter));
                footer.register(FooterHint::new("←/→", "Fold").secondary());
            }
        } else if matches!(context.target, ConfigurationTarget::Machine(_)) {
            footer.register(FooterHint::new("Enter", "Access").on_key(KeyCode::Enter));
        }
        if matches!(context.target, ConfigurationTarget::Machine(_)) {
            footer.register(FooterHint::new("c", "Access").on_key(KeyCode::Char('c')));
        }
    }

    type Action = X11PageAction;
    type Update = X11PageUpdate;

    fn descriptor(&self) -> ConfigurationPageDescriptor {
        ConfigurationPageDescriptor {
            id: PAGE_ID,
            section: crate::tui::features::configuration::pages::HOST_INTEGRATION,
            label: "X11",
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

    fn render_overlay(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        self.render_access_dialog(frame, area, foreground);
    }

    fn modal_open(&self) -> bool {
        self.access_dialog_is_open()
    }

    fn handle_input(
        &mut self,
        input: PageInput,
        context: PageInteractionContext<'_>,
    ) -> PageRequest<X11PageAction> {
        match input {
            PageInput::Key(key) => self.handle_page_key(key, context),
            PageInput::Mouse(mouse) => self
                .handle_access_mouse(mouse)
                .map_or(PageRequest::None, Into::into),
            PageInput::Click(position) => self.handle_page_click(position, context),
            PageInput::Navigate(action) => {
                self.move_focus(
                    action,
                    matches!(context.target, ConfigurationTarget::Machine(_)),
                );
                PageRequest::None
            }
        }
    }

    fn update(&mut self, update: Self::Update) {
        match update {
            X11PageUpdate::TrackCheck(task) => self.track_check(task),
            X11PageUpdate::TrackAuthorization(task) => self.track_authorization(task),
            X11PageUpdate::TrackRevocation(task) => self.track_revocation(task),
            X11PageUpdate::Checked { generation, result } => {
                if let Some(dialog) = &mut self.access_dialog {
                    dialog.finish_check(generation, result);
                }
            }
            X11PageUpdate::Authorized { generation, result } => {
                if let Some(dialog) = &mut self.access_dialog {
                    dialog.finish_authorization(generation, result);
                }
            }
            X11PageUpdate::Revoked { generation, result } => {
                if let Some(dialog) = &mut self.access_dialog {
                    dialog.finish_revocation(generation, result);
                }
            }
        }
    }
}

#[cfg(test)]
impl X11PageState {
    pub(in crate::tui::features::configuration) fn test_state(
        &self,
    ) -> crate::tui::features::configuration::pages::ChecklistTestState {
        crate::tui::features::configuration::pages::ChecklistTestState {
            selected: self.checklist.selected(),
            expanded: self.checklist.expanded.clone(),
            modal_open: self.access_dialog_is_open(),
            bindings: self.checklist.hits.bindings.clone(),
            checkboxes: self.checklist.hits.checkboxes.clone(),
            access: self.checklist.hits.access,
        }
    }
}
