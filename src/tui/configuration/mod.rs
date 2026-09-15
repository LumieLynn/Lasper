//! Configure presentation state. Host I/O is dispatched through application
//! services by the app/effects layer; no workspace selection is borrowed after
//! opening this view.

mod navigation;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::ListState;

use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationPreview, ConfigurationSnapshot,
    ConfigurationTarget, X11BindRecommendation, X11BindingChange, X11BindingDeclaration,
};
use crate::application::inspection::ResourceInspectionError;
use crate::tui::views::title_tabs::{clicked_title_tab, TitleTabHitbox};
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
    Restart(crate::domain::machine::MachineName),
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
    checkboxes: Vec<(Rect, usize)>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum X11DraftKey {
    Declaration(usize),
    Addition(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum X11ChecklistItem {
    Declaration(usize),
    Available(PathBuf),
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
    draft: BTreeMap<X11DraftKey, X11BindingChange>,
    draft_preview: DraftPreviewState,
    discard: Option<DiscardIntent>,
    restart_confirmation: Option<crate::domain::machine::MachineName>,
    saving: bool,
    apply_error: Option<String>,
    list: ListState,
    x11_items: Vec<X11ChecklistItem>,
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
            discard: None,
            restart_confirmation: None,
            saving: false,
            apply_error: None,
            list: ListState::default(),
            x11_items: Vec::new(),
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
        self.x11_items.clear();
        self.discard = None;
        self.restart_confirmation = None;
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
                self.x11_items = x11_checklist_items(&snapshot);
                self.list =
                    ListState::default().with_selected((!self.x11_items.is_empty()).then_some(0));
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
                self.restart_confirmation = match &self.target {
                    ConfigurationTarget::Machine(machine) => Some(machine.clone()),
                    ConfigurationTarget::Image(_) => None,
                };
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

    fn toggle_selected_x11(&mut self) -> ConfigurationAction {
        if self.saving {
            return ConfigurationAction::None;
        }
        let Some(item) = self
            .list
            .selected()
            .and_then(|index| self.x11_items.get(index))
            .cloned()
        else {
            return ConfigurationAction::None;
        };
        let (key, change) = match item {
            X11ChecklistItem::Declaration(line) => (
                X11DraftKey::Declaration(line),
                X11BindingChange::Remove { line },
            ),
            X11ChecklistItem::Available(source) => (
                X11DraftKey::Addition(source.clone()),
                X11BindingChange::Add { source },
            ),
        };
        if self.draft.remove(&key).is_none() {
            self.draft.insert(key, change);
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

    fn toggle_selected_details(&mut self) {
        let Some(selected) = self.list.selected() else {
            return;
        };
        if !self.expanded.remove(&selected) {
            self.expanded.insert(selected);
        }
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
        } else if self.pane == ConfigurationPane::Content && !self.x11_items.is_empty() {
            let current = self.list.selected().unwrap_or(0);
            self.list.select(Some(if down {
                (current + 1).min(self.x11_items.len() - 1)
            } else {
                current.saturating_sub(1)
            }));
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ConfigurationAction {
        if self.restart_confirmation.is_some() {
            return self.handle_restart_confirmation_key(key);
        }
        if self.discard.is_some() {
            return self.handle_discard_key(key);
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
                ConfigurationPane::Content => return self.toggle_selected_x11(),
                ConfigurationPane::Preview => {}
            },
            (KeyCode::Enter, KeyModifiers::NONE) if self.pane == ConfigurationPane::Content => {
                self.toggle_selected_details();
            }
            _ => {}
        }
        ConfigurationAction::None
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> ConfigurationAction {
        if self.discard.is_some() || self.restart_confirmation.is_some() {
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
                let toggle = self
                    .hits
                    .checkboxes
                    .iter()
                    .find(|(area, _)| area.contains(position))
                    .map(|(_, selected)| *selected);
                if let Some((_, selected)) = self
                    .hits
                    .bindings
                    .iter()
                    .find(|(area, _)| area.contains(position))
                {
                    self.list.select(Some(*selected));
                    if toggle == Some(*selected) {
                        return self.toggle_selected_x11();
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

    fn x11_item_change(&self, item: &X11ChecklistItem) -> Option<&X11BindingChange> {
        let key = match item {
            X11ChecklistItem::Declaration(line) => X11DraftKey::Declaration(*line),
            X11ChecklistItem::Available(source) => X11DraftKey::Addition(source.clone()),
        };
        self.draft.get(&key)
    }

    fn x11_item_checked(&self, item: &X11ChecklistItem) -> bool {
        match item {
            X11ChecklistItem::Declaration(_) => !matches!(
                self.x11_item_change(item),
                Some(X11BindingChange::Remove { .. })
            ),
            X11ChecklistItem::Available(_) => {
                matches!(
                    self.x11_item_change(item),
                    Some(X11BindingChange::Add { .. })
                )
            }
        }
    }
}

fn x11_checklist_items(snapshot: &ConfigurationSnapshot) -> Vec<X11ChecklistItem> {
    let mut items = snapshot
        .x11_bindings
        .iter()
        .map(|binding| X11ChecklistItem::Declaration(binding.line))
        .collect::<Vec<_>>();
    let recommended = snapshot
        .x11_bindings
        .iter()
        .filter(|binding| declaration_is_recommended(binding, &snapshot.x11_bind_recommendation))
        .map(|binding| binding.source.as_path())
        .collect::<BTreeSet<_>>();
    items.extend(
        snapshot
            .host_x11
            .sockets
            .iter()
            .filter(|socket| !recommended.contains(socket.source()))
            .map(|socket| X11ChecklistItem::Available(socket.source().to_path_buf())),
    );
    items
}

fn declaration_is_recommended(
    binding: &X11BindingDeclaration,
    recommendation: &X11BindRecommendation,
) -> bool {
    let X11BindRecommendation::Ready { idmapped, .. } = recommendation else {
        return false;
    };
    binding.readonly
        && binding.source == binding.guest_target
        && binding.options.iter().any(|option| option == "idmap") == *idmapped
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
