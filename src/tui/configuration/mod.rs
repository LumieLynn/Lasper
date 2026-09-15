//! Configure presentation state. Host I/O is dispatched through application
//! services by the app/effects layer; no workspace selection is borrowed after
//! opening this view.

mod render;

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::application::configuration::{ConfigurationSnapshot, ConfigurationTarget};
use crate::application::inspection::ResourceInspectionError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationPane {
    Navigation,
    Content,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewTab {
    Checks,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationAction {
    None,
    Close,
    Refresh,
}

enum InspectionState {
    Loading,
    Ready(ConfigurationSnapshot),
    Failed(String),
}

#[derive(Default)]
struct HitAreas {
    navigation: Rect,
    content: Rect,
    preview: Rect,
    checks_tab: Rect,
    raw_tab: Rect,
    refresh: Rect,
    close: Rect,
    bindings: Vec<(Rect, usize)>,
}

pub(crate) struct ConfigurationView {
    pub(crate) target: ConfigurationTarget,
    query: u64,
    pending: Option<tokio::task::JoinHandle<()>>,
    state: InspectionState,
    pane: ConfigurationPane,
    preview_tab: PreviewTab,
    list: ListState,
    expanded: BTreeSet<usize>,
    preview_scroll: usize,
    preview_max_scroll: usize,
    preview_width: u16,
    preview_cache: Option<Vec<String>>,
    hits: HitAreas,
}

impl ConfigurationView {
    pub(crate) fn new(target: ConfigurationTarget) -> Self {
        Self {
            target,
            query: 0,
            pending: None,
            state: InspectionState::Loading,
            pane: ConfigurationPane::Content,
            preview_tab: PreviewTab::Checks,
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
        self.state = InspectionState::Loading;
        self.preview_cache = None;
    }

    pub(crate) fn track_query(&mut self, task: tokio::task::JoinHandle<()>) {
        self.pending = Some(task);
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
                InspectionState::Ready(snapshot)
            }
            Ok(_) => InspectionState::Failed("Inspection returned a different resource".into()),
            Err(error) => InspectionState::Failed(error.to_string()),
        };
        self.preview_scroll = 0;
        self.preview_cache = None;
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
        if self.pane == ConfigurationPane::Preview {
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
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return ConfigurationAction::Close,
            (KeyCode::Tab, modifiers) => self.cycle_pane(modifiers.contains(KeyModifiers::SHIFT)),
            (KeyCode::BackTab, _) => self.cycle_pane(true),
            (KeyCode::Char('r'), KeyModifiers::NONE) => return ConfigurationAction::Refresh,
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => self.scroll(true),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => self.scroll(false),
            (KeyCode::Char('[') | KeyCode::Char(']'), KeyModifiers::NONE)
                if self.pane == ConfigurationPane::Preview =>
            {
                self.select_tab(match self.preview_tab {
                    PreviewTab::Checks => PreviewTab::Raw,
                    PreviewTab::Raw => PreviewTab::Checks,
                });
            }
            (KeyCode::Enter, KeyModifiers::NONE) => match self.pane {
                ConfigurationPane::Navigation => self.pane = ConfigurationPane::Content,
                ConfigurationPane::Content => {
                    if let Some(selected) = self.list.selected() {
                        if !self.expanded.remove(&selected) {
                            self.expanded.insert(selected);
                        }
                    }
                }
                ConfigurationPane::Preview => {}
            },
            _ => {}
        }
        ConfigurationAction::None
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> ConfigurationAction {
        let position = (mouse.column, mouse.row).into();
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if self.hits.refresh.contains(position) {
                return ConfigurationAction::Refresh;
            }
            if self.hits.close.contains(position) {
                return ConfigurationAction::Close;
            }
            if self.hits.checks_tab.contains(position) {
                self.select_tab(PreviewTab::Checks);
            } else if self.hits.raw_tab.contains(position) {
                self.select_tab(PreviewTab::Raw);
            } else if self.hits.navigation.contains(position) {
                self.pane = ConfigurationPane::Navigation;
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
    }
}

#[cfg(test)]
mod tests;
