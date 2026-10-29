//! Configure presentation state. Host I/O is dispatched through application
//! services by the app/effects layer; no workspace selection is borrowed after
//! opening this view.

mod footer;
mod interaction;
mod lifecycle;
mod navigation;
mod preview;
mod render;

use ratatui::layout::Rect;

use super::pages::{ConfigurationPageAction, ConfigurationPages};
use crate::application::configuration::{
    ConfigurationDraft, ConfigurationEdit, ConfigurationPreview, ConfigurationTarget,
};
use crate::tui::features::configuration::page::{ConfigurationPane, InspectionState};
use navigation::ConfigurationNavigation;
use preview::{PreviewPane, PreviewTab};

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
    draft_generation: u64,
    draft_preview: DraftPreviewState,
    discard: Option<DiscardIntent>,
    restart_confirmation: Option<crate::domain::machine::MachineName>,
    saving: bool,
    apply_requires_refresh: bool,
    apply_error: Option<String>,
    pages: ConfigurationPages,
    draft: ConfigurationDraft,
    preview: PreviewPane,
    hits: HitAreas,
}

impl ConfigurationView {
    pub(crate) fn new(target: ConfigurationTarget) -> Self {
        let pages = ConfigurationPages::default();
        let navigation = ConfigurationNavigation::new(pages.descriptors());
        let mut view = Self {
            target,
            query: 0,
            pending: None,
            pending_preview: None,
            pending_apply: None,
            state: InspectionState::Loading,
            pane: ConfigurationPane::Content,
            navigation,
            draft_generation: 0,
            draft_preview: DraftPreviewState::Clean,
            discard: None,
            restart_confirmation: None,
            saving: false,
            apply_requires_refresh: false,
            apply_error: None,
            pages,
            draft: ConfigurationDraft::default(),
            preview: PreviewPane::default(),
            hits: HitAreas::default(),
        };
        view.with_preview(|_| ());
        view
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
