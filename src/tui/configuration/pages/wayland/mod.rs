//! Wayland configuration page state and local projection.

use std::path::PathBuf;

use super::checklist::PageChecklist;
use crate::application::configuration::{
    recommended_wayland_target, ConfigurationSnapshot, WaylandSourceState,
};
use crate::tui::widgets::dialogs::wayland_session::WaylandSessionDialog;

mod controller;
mod effects;
mod presentation;
mod runtime;

pub(crate) use controller::WaylandPageUpdate;
pub(in crate::tui::configuration) use effects::start_action;
pub(crate) use effects::WaylandPageEvent;
pub(crate) use runtime::WaylandPageAction;

pub(in crate::tui::configuration) const PAGE_ID:
    crate::tui::configuration::core::page::ConfigurationPageId =
    crate::tui::configuration::core::page::ConfigurationPageId::Wayland;

#[derive(Clone, Debug, PartialEq, Eq)]
enum WaylandChecklistItem {
    Declaration(usize),
    Available(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WaylandDraftItem {
    Declaration(usize),
    Available {
        source: PathBuf,
        guest_target: PathBuf,
    },
}

#[derive(Default)]
pub(super) struct WaylandPageState {
    access_dialog: Option<WaylandSessionDialog>,
    checklist: PageChecklist<WaylandChecklistItem>,
}

impl WaylandPageState {
    pub(super) fn begin_query(&mut self) {
        self.access_dialog = None;
        self.checklist.reset();
    }

    pub(super) fn finish_query(&mut self, snapshot: &ConfigurationSnapshot) {
        self.checklist.load(checklist_items(snapshot));
    }

    pub(super) fn selected_item(
        &self,
        snapshot: &ConfigurationSnapshot,
    ) -> Option<WaylandDraftItem> {
        match self
            .checklist
            .selected()
            .and_then(|index| self.checklist.items.get(index))?
        {
            WaylandChecklistItem::Declaration(line) => Some(WaylandDraftItem::Declaration(*line)),
            WaylandChecklistItem::Available(source) => {
                let socket = snapshot
                    .host_wayland
                    .sockets
                    .iter()
                    .find(|socket| socket.canonical_path() == source)?;
                Some(WaylandDraftItem::Available {
                    source: source.clone(),
                    guest_target: recommended_wayland_target(socket.owner_uid(), socket.display()),
                })
            }
        }
    }

    pub(super) fn toggle_selected_details(&mut self) {
        self.checklist.toggle_selected_expansion();
    }

    pub(super) fn select_item(&mut self, index: usize) {
        self.checklist.select(index);
    }

    pub(super) fn move_focus(&mut self, down: bool, runtime_access_available: bool) {
        self.checklist.move_focus(down, runtime_access_available);
    }

    pub(super) fn is_bindings_focused(&self) -> bool {
        self.checklist.bindings_focused()
    }

    pub(super) fn set_bindings_focus(&mut self) {
        self.checklist.focus_bindings();
    }

    pub(super) fn set_runtime_access_focus(&mut self) {
        self.checklist.focus_runtime();
    }
}

fn checklist_items(snapshot: &ConfigurationSnapshot) -> Vec<WaylandChecklistItem> {
    let mut items = snapshot
        .wayland_bindings
        .iter()
        .map(|binding| WaylandChecklistItem::Declaration(binding.line))
        .collect::<Vec<_>>();
    items.extend(
        snapshot
            .host_wayland
            .sockets
            .iter()
            .filter(|socket| {
                !snapshot.wayland_bindings.iter().any(|binding| {
                    binding.source == socket.canonical_path()
                        || (binding.display == *socket.display()
                            && snapshot.host_wayland.sources.iter().any(|observation| {
                                observation.source == binding.source
                                    && observation.state == WaylandSourceState::Observed
                            }))
                })
            })
            .map(|socket| WaylandChecklistItem::Available(socket.canonical_path().to_path_buf())),
    );
    items
}
