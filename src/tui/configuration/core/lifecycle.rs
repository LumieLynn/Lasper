//! Inspection, preview, and apply lifecycle for the configuration workspace.

use super::{ConfigurationView, DraftPreviewState, InspectionState};
use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationError, ConfigurationPreview,
    ConfigurationSnapshot, ConfigurationTarget,
};
use crate::tui::configuration::page::ConfigurationPageId;
use crate::tui::configuration::pages::{ConfigurationPageEvent, ConfigurationPageUpdate};

impl ConfigurationView {
    pub(crate) fn begin_query(&mut self, query: u64) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
        self.pages.reset();
        self.query = query;
        self.cancel_preview();
        self.draft.clear();
        self.draft_preview = DraftPreviewState::Clean;
        self.discard = None;
        self.restart_confirmation = None;
        self.apply_error = None;
        self.apply_requires_refresh = false;
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

    pub(crate) fn update_page(&mut self, update: ConfigurationPageUpdate) {
        self.pages.update(update);
    }

    pub(crate) fn finish_page_event(
        &mut self,
        event: ConfigurationPageEvent,
    ) -> Result<(), ConfigurationPageEvent> {
        if event.target() != &self.target {
            return Err(event);
        }
        self.pages.update(event.into_update());
        Ok(())
    }

    pub(crate) fn reject_page_action(
        &mut self,
        page: ConfigurationPageId,
        message: impl Into<String>,
    ) {
        self.pages.reject_action(page, message.into());
    }

    pub(crate) fn finish_query(
        &mut self,
        query: u64,
        target: &ConfigurationTarget,
        result: Result<ConfigurationSnapshot, ConfigurationError>,
    ) {
        if self.query != query || &self.target != target {
            return;
        }
        self.pending.take();
        self.state = match result {
            Ok(snapshot) if snapshot.target == self.target => {
                self.pages.load(&snapshot);
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
        result: Result<ConfigurationPreview, ConfigurationError>,
    ) {
        if generation != self.draft_generation || target != &self.target || self.draft_is_empty() {
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
        result: Result<ConfigurationApplyReport, ConfigurationError>,
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
                Some("Configuration saved; it takes effect on the next machine start".into())
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
                self.apply_requires_refresh = error.is_outcome_unknown();
                self.apply_error = Some(error.to_string());
                self.preview_cache = None;
                None
            }
        }
    }

    pub(super) fn cancel_preview(&mut self) {
        if let Some(task) = self.pending_preview.take() {
            task.abort();
        }
    }

    pub(super) fn current_edit(&self) -> Option<ConfigurationEdit> {
        let InspectionState::Ready(snapshot) = &self.state else {
            return None;
        };
        self.draft.edit(&self.target, snapshot)
    }

    pub(super) fn draft_is_empty(&self) -> bool {
        self.draft.is_empty()
    }
}
