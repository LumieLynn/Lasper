//! Keyboard and pointer state machine for the configuration workspace.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::{
    ConfigurationAction, ConfigurationPane, ConfigurationView, DiscardIntent, DraftPreviewState,
    InspectionState, PreviewTab,
};
use crate::application::configuration::ConfigurationPreview;
use crate::tui::configuration::page::PageRequest;
use crate::tui::configuration::page::{PageInput, PageInteractionContext};
use crate::tui::configuration::pages::ConfigurationPageAction;
use crate::tui::views::title_tabs::clicked_title_tab;

impl ConfigurationView {
    fn dispatch_page_input(&mut self, input: PageInput) -> ConfigurationAction {
        let snapshot = match &self.state {
            InspectionState::Ready(snapshot) => Some(snapshot.as_ref()),
            InspectionState::Loading | InspectionState::Failed(_) => None,
        };
        let request = self.pages.handle_input(
            self.navigation.current_page(),
            input,
            PageInteractionContext {
                target: &self.target,
                snapshot,
            },
        );
        self.resolve_page_request(request)
    }

    fn resolve_page_request(
        &mut self,
        request: PageRequest<ConfigurationPageAction>,
    ) -> ConfigurationAction {
        match request {
            PageRequest::None => ConfigurationAction::None,
            PageRequest::Draft(_) if self.saving || self.apply_requires_refresh => {
                ConfigurationAction::None
            }
            PageRequest::Draft(request) => {
                self.draft.apply(request);
                self.draft_changed()
            }
            PageRequest::CleanDraftAction {
                blocked_message, ..
            } if !self.draft_is_empty() => {
                self.pages
                    .reject_action(self.navigation.current_page(), blocked_message);
                ConfigurationAction::None
            }
            PageRequest::CleanDraftAction { action, .. } | PageRequest::Action(action) => {
                ConfigurationAction::Page(action)
            }
        }
    }

    fn draft_changed(&mut self) -> ConfigurationAction {
        self.cancel_preview();
        self.draft_generation = self
            .draft_generation
            .checked_add(1)
            .expect("configuration draft generation exhausted");
        self.apply_error = None;
        self.preview_scroll = 0;
        self.preview_cache = None;
        if self.draft_is_empty() {
            self.draft_preview = DraftPreviewState::Clean;
            return ConfigurationAction::None;
        }
        let Some(edit) = self.current_edit() else {
            self.draft_preview = DraftPreviewState::Failed {
                generation: self.draft_generation,
                message: "Configuration has no complete revision; refresh Checks before editing"
                    .into(),
            };
            return ConfigurationAction::None;
        };
        self.preview_tab = PreviewTab::Diff;
        self.draft_preview = DraftPreviewState::Loading(self.draft_generation);
        ConfigurationAction::Preview {
            generation: self.draft_generation,
            edit,
        }
    }

    fn active_access_dialog_is_open(&self) -> bool {
        self.pages.modal_open(self.navigation.current_page())
    }

    fn request_close_or_refresh(&mut self, intent: DiscardIntent) -> ConfigurationAction {
        if self.saving {
            self.apply_error = Some("A configuration save is still in progress".into());
            self.preview_cache = None;
            return ConfigurationAction::None;
        }
        if !self.draft_is_empty() {
            self.discard = Some(intent);
            return ConfigurationAction::None;
        }
        match intent {
            DiscardIntent::Close => ConfigurationAction::Close,
            DiscardIntent::Refresh => ConfigurationAction::Refresh,
        }
    }

    fn handle_discard_key(&mut self, key: KeyEvent) -> ConfigurationAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let intent = self.discard.take().expect("discard intent is visible");
                self.draft.clear();
                match intent {
                    DiscardIntent::Close => ConfigurationAction::Close,
                    DiscardIntent::Refresh => ConfigurationAction::Refresh,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.discard = None;
                ConfigurationAction::None
            }
            _ => ConfigurationAction::None,
        }
    }

    fn handle_restart_confirmation_key(&mut self, key: KeyEvent) -> ConfigurationAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self
                .restart_confirmation
                .take()
                .map_or(ConfigurationAction::None, ConfigurationAction::Restart),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.restart_confirmation = None;
                ConfigurationAction::Refresh
            }
            _ => ConfigurationAction::None,
        }
    }

    pub(crate) fn restart_confirmation_pending(&self) -> bool {
        self.restart_confirmation.is_some()
    }

    fn cycle_pane(&mut self, reverse: bool) {
        use ConfigurationPane::*;
        self.pane = match (self.pane, reverse) {
            (Navigation, false) | (Preview, true) => Content,
            (Content, false) | (Navigation, true) => Preview,
            (Preview, false) | (Content, true) => Navigation,
        };
    }

    pub(super) fn select_tab(&mut self, tab: PreviewTab) {
        self.pane = ConfigurationPane::Preview;
        if self.preview_tab != tab {
            self.preview_tab = tab;
            self.preview_scroll = 0;
            self.preview_cache = None;
        }
    }

    pub(super) fn scroll(&mut self, down: bool) {
        if self.pane == ConfigurationPane::Navigation {
            self.navigation.move_selection(down);
        } else if self.pane == ConfigurationPane::Preview {
            self.preview_scroll = if down {
                self.preview_scroll
                    .saturating_add(1)
                    .min(self.preview_max_scroll)
            } else {
                self.preview_scroll.saturating_sub(1)
            };
        } else if self.pane == ConfigurationPane::Content {
            let _ = self.dispatch_page_input(PageInput::Scroll(down));
        }
    }

    fn scroll_preview_page(&mut self, down: bool) {
        if self.pane != ConfigurationPane::Preview {
            return;
        }
        let viewport = usize::from(self.hits.preview.height.saturating_sub(2));
        let step = viewport.saturating_sub(1).max(1);
        self.preview_scroll = if down {
            self.preview_scroll
                .saturating_add(step)
                .min(self.preview_max_scroll)
        } else {
            self.preview_scroll.saturating_sub(step)
        };
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ConfigurationAction {
        if self.active_access_dialog_is_open() {
            return self.dispatch_page_input(PageInput::Key(key));
        }
        if self.restart_confirmation.is_some() {
            return self.handle_restart_confirmation_key(key);
        }
        if self.discard.is_some() {
            return self.handle_discard_key(key);
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('?'), _) => return ConfigurationAction::Help,
            (KeyCode::Esc, _) => {
                return self.request_close_or_refresh(DiscardIntent::Close);
            }
            (KeyCode::Char('s'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                if self.saving || self.apply_requires_refresh {
                    return ConfigurationAction::None;
                }
                let ready = matches!(
                    self.draft_preview,
                    DraftPreviewState::Ready {
                        generation,
                        preview: ConfigurationPreview::Ready { .. }
                    } if generation == self.draft_generation
                );
                if ready {
                    if let Some(edit) = self.current_edit() {
                        self.saving = true;
                        self.apply_error = None;
                        self.preview_cache = None;
                        return ConfigurationAction::Apply {
                            generation: self.draft_generation,
                            edit,
                        };
                    }
                }
            }
            (KeyCode::Tab, modifiers) => self.cycle_pane(modifiers.contains(KeyModifiers::SHIFT)),
            (KeyCode::BackTab, _) => self.cycle_pane(true),
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                return self.request_close_or_refresh(DiscardIntent::Refresh);
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => self.scroll(true),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => self.scroll(false),
            (KeyCode::PageDown, KeyModifiers::NONE) => self.scroll_preview_page(true),
            (KeyCode::PageUp, KeyModifiers::NONE) => self.scroll_preview_page(false),
            (KeyCode::Char('[') | KeyCode::Char(']'), KeyModifiers::NONE)
                if self.pane == ConfigurationPane::Preview =>
            {
                self.select_tab(self.preview_tab.adjacent(key.code == KeyCode::Char(']')));
            }
            (_, KeyModifiers::NONE) if self.pane == ConfigurationPane::Navigation => {
                if self.navigation.handle_key(key.code) {
                    self.pane = ConfigurationPane::Content;
                }
            }
            (
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char(' ')
                | KeyCode::Enter
                | KeyCode::Char('c'),
                KeyModifiers::NONE,
            ) if self.pane == ConfigurationPane::Content => {
                return self.dispatch_page_input(PageInput::Key(key));
            }
            _ => {}
        }
        ConfigurationAction::None
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> ConfigurationAction {
        if self.active_access_dialog_is_open()
            || self.discard.is_some()
            || self.restart_confirmation.is_some()
        {
            return ConfigurationAction::None;
        }
        let position = (mouse.column, mouse.row).into();
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if self.hits.refresh.contains(position) {
                return self.request_close_or_refresh(DiscardIntent::Refresh);
            }
            if self.hits.close.contains(position) {
                return self.request_close_or_refresh(DiscardIntent::Close);
            }
            if let Some(tab) = clicked_title_tab(&self.hits.preview_tabs, mouse) {
                self.select_tab(tab);
            } else if self.hits.navigation.contains(position) {
                self.pane = ConfigurationPane::Navigation;
                self.navigation.click(position);
            } else if self.hits.content.contains(position) {
                self.pane = ConfigurationPane::Content;
                return self.dispatch_page_input(PageInput::Click(position));
            } else if self.hits.preview.contains(position) {
                self.pane = ConfigurationPane::Preview;
            }
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if self.hits.preview.contains(position) {
                self.pane = ConfigurationPane::Preview;
            } else if self.hits.content.contains(position) {
                self.pane = ConfigurationPane::Content;
            } else if self.hits.navigation.contains(position) {
                self.pane = ConfigurationPane::Navigation;
            } else {
                return ConfigurationAction::None;
            }
            self.scroll(mouse.kind == MouseEventKind::ScrollDown);
        }
        ConfigurationAction::None
    }
}
