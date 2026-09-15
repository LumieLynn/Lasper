//! Configure presentation state. Host I/O is dispatched through application
//! services by the app/effects layer; no workspace selection is borrowed after
//! opening this view.

mod editor;
mod navigation;
mod render;

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::ListState;

use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationPreview, ConfigurationSnapshot,
    ConfigurationTarget, X11BindingChange,
};
use crate::application::inspection::ResourceInspectionError;
use crate::tui::views::title_tabs::{clicked_title_tab, TitleTabHitbox};
use editor::{EditorOutcome, X11BindingEditor};
use navigation::ConfigurationNavigation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationPane {
    Navigation,
    Content,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewTab {
    Diff,
    Checks,
    Raw,
}

impl PreviewTab {
    const ALL: [Self; 3] = [Self::Diff, Self::Raw, Self::Checks];

    fn label(self) -> &'static str {
        match self {
            Self::Diff => " Diff ",
            Self::Raw => " Raw ",
            Self::Checks => " Checks ",
        }
    }

    fn adjacent(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("preview tab is listed");
        Self::ALL[(index + if forward { 1 } else { Self::ALL.len() - 1 }) % Self::ALL.len()]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationAction {
    None,
    Close,
    Refresh,
    Preview {
        generation: u64,
        edit: ConfigurationEdit,
    },
    Apply {
        generation: u64,
        edit: ConfigurationEdit,
    },
}

enum InspectionState {
    Loading,
    Ready(Box<ConfigurationSnapshot>),
    Failed(String),
}

enum DraftPreviewState {
    Clean,
    Loading(u64),
    Ready {
        generation: u64,
        preview: ConfigurationPreview,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

#[derive(Clone, Copy)]
enum DiscardIntent {
    Close,
    Refresh,
}

#[derive(Default)]
struct HitAreas {
    navigation: Rect,
    content: Rect,
    preview: Rect,
    preview_tabs: Vec<TitleTabHitbox<PreviewTab>>,
    refresh: Rect,
    close: Rect,
    bindings: Vec<(Rect, usize)>,
}

pub(crate) struct ConfigurationView {
    pub(crate) target: ConfigurationTarget,
    query: u64,
    pending: Option<tokio::task::JoinHandle<()>>,
    pending_preview: Option<tokio::task::JoinHandle<()>>,
    pending_apply: Option<tokio::task::JoinHandle<()>>,
    state: InspectionState,
    pane: ConfigurationPane,
    navigation: ConfigurationNavigation,
    preview_tab: PreviewTab,
    draft_generation: u64,
    draft: BTreeMap<usize, X11BindingChange>,
    draft_preview: DraftPreviewState,
    editor: Option<X11BindingEditor>,
    discard: Option<DiscardIntent>,
    saving: bool,
    apply_error: Option<String>,
    list: ListState,
    expanded: BTreeSet<usize>,
    preview_scroll: usize,
    preview_max_scroll: usize,
    preview_width: u16,
    preview_cache: Option<Vec<Line<'static>>>,
    hits: HitAreas,
}

impl ConfigurationView {
    pub(crate) fn new(target: ConfigurationTarget) -> Self {
        Self {
            target,
            query: 0,
            pending: None,
            pending_preview: None,
            pending_apply: None,
            state: InspectionState::Loading,
            pane: ConfigurationPane::Content,
            navigation: ConfigurationNavigation::default(),
            preview_tab: PreviewTab::Checks,
            draft_generation: 0,
            draft: BTreeMap::new(),
            draft_preview: DraftPreviewState::Clean,
            editor: None,
            discard: None,
            saving: false,
            apply_error: None,
            list: ListState::default(),
            expanded: BTreeSet::new(),
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_width: 0,
            preview_cache: None,
            hits: HitAreas::default(),
        }
    }

    pub(crate) fn begin_query(&mut self, query: u64) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
        self.query = query;
        self.cancel_preview();
        self.draft.clear();
        self.draft_preview = DraftPreviewState::Clean;
        self.editor = None;
        self.discard = None;
        self.apply_error = None;
        self.state = InspectionState::Loading;
        self.preview_cache = None;
    }

    pub(crate) fn track_query(&mut self, task: tokio::task::JoinHandle<()>) {
        self.pending = Some(task);
    }

    pub(crate) fn track_preview(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(previous) = self.pending_preview.replace(task) {
            previous.abort();
        }
    }

    pub(crate) fn track_apply(&mut self, task: tokio::task::JoinHandle<()>) {
        self.pending_apply = Some(task);
    }

    pub(crate) fn finish_query(
        &mut self,
        query: u64,
        target: &ConfigurationTarget,
        result: Result<ConfigurationSnapshot, ResourceInspectionError>,
    ) {
        if self.query != query || &self.target != target {
            return;
        }
        self.pending.take();
        self.state = match result {
            Ok(snapshot) if snapshot.target == self.target => {
                self.list = ListState::default()
                    .with_selected((!snapshot.x11_bindings.is_empty()).then_some(0));
                self.expanded.clear();
                self.expanded.insert(0);
                InspectionState::Ready(Box::new(snapshot))
            }
            Ok(_) => InspectionState::Failed("Inspection returned a different resource".into()),
            Err(error) => InspectionState::Failed(error.to_string()),
        };
        self.preview_scroll = 0;
        self.preview_cache = None;
    }

    pub(crate) fn finish_preview(
        &mut self,
        generation: u64,
        target: &ConfigurationTarget,
        result: Result<ConfigurationPreview, ResourceInspectionError>,
    ) {
        if generation != self.draft_generation || target != &self.target || self.draft.is_empty() {
            return;
        }
        self.pending_preview.take();
        self.draft_preview = match result {
            Ok(preview) => DraftPreviewState::Ready {
                generation,
                preview,
            },
            Err(error) => DraftPreviewState::Failed {
                generation,
                message: error.to_string(),
            },
        };
        self.preview_cache = None;
    }

    pub(crate) fn finish_apply(
        &mut self,
        generation: u64,
        target: &ConfigurationTarget,
        result: Result<ConfigurationApplyReport, ResourceInspectionError>,
    ) -> Option<String> {
        if generation != self.draft_generation || target != &self.target || !self.saving {
            return None;
        }
        self.pending_apply.take();
        self.saving = false;
        match result {
            Ok(ConfigurationApplyReport::Applied { .. }) => {
                self.draft.clear();
                self.draft_preview = DraftPreviewState::Clean;
                self.apply_error = None;
                Some("X11 configuration saved; it takes effect on the next machine start".into())
            }
            Ok(ConfigurationApplyReport::Unchanged { .. }) => {
                self.draft.clear();
                self.draft_preview = DraftPreviewState::Clean;
                self.apply_error = None;
                Some("Configuration is already up to date".into())
            }
            Ok(
                ConfigurationApplyReport::Blocked { reason }
                | ConfigurationApplyReport::Conflict { reason }
                | ConfigurationApplyReport::Busy { reason },
            ) => {
                self.apply_error = Some(reason);
                self.preview_cache = None;
                None
            }
            Err(error) => {
                self.apply_error = Some(error.to_string());
                self.preview_cache = None;
                None
            }
        }
    }

    fn cancel_preview(&mut self) {
        if let Some(task) = self.pending_preview.take() {
            task.abort();
        }
    }

    fn current_edit(&self) -> Option<ConfigurationEdit> {
        let InspectionState::Ready(snapshot) = &self.state else {
            return None;
        };
        Some(ConfigurationEdit {
            target: self.target.clone(),
            base_revision: snapshot.revision.clone()?,
            x11_changes: self.draft.values().cloned().collect(),
        })
    }

    fn set_change(&mut self, change: X11BindingChange) -> ConfigurationAction {
        let line = change
            .declaration_line()
            .expect("the current editor changes existing declarations");
        let unchanged = match (&change, &self.state) {
            (
                X11BindingChange::Update {
                    source,
                    guest_target,
                    readonly,
                    ..
                },
                InspectionState::Ready(snapshot),
            ) => snapshot.x11_bindings.iter().any(|binding| {
                binding.line == line
                    && &binding.source == source
                    && &binding.guest_target == guest_target
                    && binding.readonly == *readonly
            }),
            _ => false,
        };
        if unchanged {
            self.draft.remove(&line);
        } else {
            self.draft.insert(line, change);
        }
        self.draft_changed()
    }

    fn clear_selected_change(&mut self) -> ConfigurationAction {
        let Some(line) = self.selected_binding().map(|binding| binding.line) else {
            return ConfigurationAction::None;
        };
        if self.draft.remove(&line).is_none() {
            return ConfigurationAction::None;
        }
        self.draft_changed()
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
        if self.draft.is_empty() {
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

    fn selected_binding(
        &self,
    ) -> Option<&crate::application::configuration::X11BindingDeclaration> {
        let InspectionState::Ready(snapshot) = &self.state else {
            return None;
        };
        snapshot.x11_bindings.get(self.list.selected()?)
    }

    fn open_editor(&mut self) {
        if self.saving {
            return;
        }
        let Some(binding) = self.selected_binding() else {
            return;
        };
        let line = binding.line;
        let editor = X11BindingEditor::new(binding, self.draft.get(&line));
        self.editor = Some(editor);
    }

    fn request_close_or_refresh(&mut self, intent: DiscardIntent) -> ConfigurationAction {
        if self.saving {
            self.apply_error = Some("A configuration save is still in progress".into());
            self.preview_cache = None;
            return ConfigurationAction::None;
        }
        if !self.draft.is_empty() {
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

    fn cycle_pane(&mut self, reverse: bool) {
        use ConfigurationPane::*;
        self.pane = match (self.pane, reverse) {
            (Navigation, false) | (Preview, true) => Content,
            (Content, false) | (Navigation, true) => Preview,
            (Preview, false) | (Content, true) => Navigation,
        };
    }

    fn select_tab(&mut self, tab: PreviewTab) {
        self.pane = ConfigurationPane::Preview;
        if self.preview_tab != tab {
            self.preview_tab = tab;
            self.preview_scroll = 0;
            self.preview_cache = None;
        }
    }

    fn scroll(&mut self, down: bool) {
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
            if let InspectionState::Ready(snapshot) = &self.state {
                if snapshot.x11_bindings.is_empty() {
                    return;
                }
                let current = self.list.selected().unwrap_or(0);
                self.list.select(Some(if down {
                    (current + 1).min(snapshot.x11_bindings.len() - 1)
                } else {
                    current.saturating_sub(1)
                }));
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ConfigurationAction {
        if self.discard.is_some() {
            return self.handle_discard_key(key);
        }
        if let Some(editor) = &mut self.editor {
            let outcome = editor.handle_key(key);
            return match outcome {
                EditorOutcome::None => ConfigurationAction::None,
                EditorOutcome::Cancel => {
                    self.editor = None;
                    ConfigurationAction::None
                }
                EditorOutcome::Submit(change) => {
                    self.editor = None;
                    self.set_change(change)
                }
            };
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                return self.request_close_or_refresh(DiscardIntent::Close);
            }
            (KeyCode::Char('s'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                if self.saving {
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
            (KeyCode::Char('e'), KeyModifiers::NONE) if self.pane == ConfigurationPane::Content => {
                self.open_editor();
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) if self.pane == ConfigurationPane::Content => {
                if let Some(line) = self.selected_binding().map(|binding| binding.line) {
                    return self.set_change(X11BindingChange::Remove { line });
                }
            }
            (KeyCode::Char('u'), KeyModifiers::NONE) if self.pane == ConfigurationPane::Content => {
                return self.clear_selected_change();
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => self.scroll(true),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => self.scroll(false),
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
            (KeyCode::Left | KeyCode::Right, KeyModifiers::NONE)
                if self.pane == ConfigurationPane::Content =>
            {
                if let Some(selected) = self.list.selected() {
                    if key.code == KeyCode::Right {
                        self.expanded.insert(selected);
                    } else {
                        self.expanded.remove(&selected);
                    }
                }
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) => match self.pane {
                ConfigurationPane::Navigation => {}
                ConfigurationPane::Content => {
                    if let Some(selected) = self.list.selected() {
                        if !self.expanded.remove(&selected) {
                            self.expanded.insert(selected);
                        }
                    }
                }
                ConfigurationPane::Preview => {}
            },
            (KeyCode::Enter, KeyModifiers::NONE) if self.pane == ConfigurationPane::Content => {
                self.open_editor();
            }
            _ => {}
        }
        ConfigurationAction::None
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> ConfigurationAction {
        if self.discard.is_some() {
            return ConfigurationAction::None;
        }
        if let Some(editor) = &mut self.editor {
            let outcome = editor.handle_mouse(mouse);
            return match outcome {
                EditorOutcome::None => ConfigurationAction::None,
                EditorOutcome::Cancel => {
                    self.editor = None;
                    ConfigurationAction::None
                }
                EditorOutcome::Submit(change) => {
                    self.editor = None;
                    self.set_change(change)
                }
            };
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
                if let Some((_, selected)) = self
                    .hits
                    .bindings
                    .iter()
                    .find(|(area, _)| area.contains(position))
                {
                    self.list.select(Some(*selected));
                    if !self.expanded.remove(selected) {
                        self.expanded.insert(*selected);
                    }
                }
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

impl Drop for ConfigurationView {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
        if let Some(task) = self.pending_preview.take() {
            task.abort();
        }
        // Applying is a mutation. Dropping a JoinHandle detaches it so direct
        // and elevated execution can still reach a definite outcome.
        self.pending_apply.take();
    }
}

#[cfg(test)]
mod tests;
