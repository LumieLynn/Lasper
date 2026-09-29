//! Configure presentation state. Host I/O is dispatched through application
//! services by the app/effects layer; no workspace selection is borrowed after
//! opening this view.

mod interaction;
mod lifecycle;
mod navigation;
mod render;

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::pages::{ConfigurationPageAction, ConfigurationPages};
use crate::application::configuration::{
    ConfigurationDraft, ConfigurationEdit, ConfigurationPreview, ConfigurationTarget,
};
use crate::tui::configuration::page::{ConfigurationPane, InspectionState};
use crate::tui::views::title_tabs::TitleTabHitbox;
use navigation::ConfigurationNavigation;

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
    Help,
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
    Page(ConfigurationPageAction),
    Restart(crate::domain::machine::MachineName),
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
    draft_preview: DraftPreviewState,
    discard: Option<DiscardIntent>,
    restart_confirmation: Option<crate::domain::machine::MachineName>,
    saving: bool,
    apply_error: Option<String>,
    pages: ConfigurationPages,
    draft: ConfigurationDraft,
    preview_scroll: usize,
    preview_max_scroll: usize,
    preview_width: u16,
    preview_cache: Option<Vec<Line<'static>>>,
    hits: HitAreas,
}

impl ConfigurationView {
    pub(crate) fn new(target: ConfigurationTarget) -> Self {
        let pages = ConfigurationPages::default();
        let navigation = ConfigurationNavigation::new(pages.descriptors());
        Self {
            target,
            query: 0,
            pending: None,
            pending_preview: None,
            pending_apply: None,
            state: InspectionState::Loading,
            pane: ConfigurationPane::Content,
            navigation,
            preview_tab: PreviewTab::Checks,
            draft_generation: 0,
            draft_preview: DraftPreviewState::Clean,
            discard: None,
            restart_confirmation: None,
            saving: false,
            apply_error: None,
            pages,
            draft: ConfigurationDraft::default(),
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_width: 0,
            preview_cache: None,
            hits: HitAreas::default(),
        }
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
        // Applying is a mutation. Dropping the JoinHandle detaches it so direct
        // and elevated execution can still reach a definite outcome.
        self.pending_apply.take();
    }
}

#[cfg(test)]
mod tests;
