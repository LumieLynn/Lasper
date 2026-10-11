//! X11 configuration page state and local projection.
//!
//! Workspace input, runtime access, and presentation are deliberately kept in
//! sibling modules so extending this page does not grow another monolithic UI
//! implementation.

use std::collections::BTreeSet;
use std::path::PathBuf;

use self::dialog::X11AuthorizationDialog;
use super::checklist::PageChecklist;
use crate::application::configuration::{
    ConfigurationDraft, ConfigurationSnapshot, X11ProjectionScope,
};
use crate::domain::x11::X11_SOCKET_DIRECTORY;
use crate::tui::features::configuration::page::ConfigurationPageId;
use crate::tui::foundation::scrolling::ScrollAction;

mod controller;
pub(in crate::tui::features::configuration) mod dialog;
mod effects;
mod presentation;
mod request;
mod runtime;

pub(in crate::tui::features::configuration) use effects::start_action;
pub(crate) use request::{X11PageAction, X11PageEvent, X11PageUpdate};

pub(in crate::tui::features::configuration) const PAGE_ID: ConfigurationPageId =
    ConfigurationPageId::new("x11");

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum X11ChecklistItem {
    Declaration(usize),
    Directory(PathBuf),
    Available(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum X11DraftItem {
    Declaration(usize),
    Directory(PathBuf),
    Available(PathBuf),
}

#[derive(Default)]
pub(super) struct X11PageState {
    access_dialog: Option<X11AuthorizationDialog>,
    checklist: PageChecklist<X11ChecklistItem>,
}

impl X11PageState {
    pub(super) fn begin_query(&mut self) {
        self.access_dialog = None;
        self.checklist.reset();
    }

    pub(super) fn finish_query(&mut self, snapshot: &ConfigurationSnapshot) {
        self.checklist.load(checklist_items(snapshot));
    }

    pub(super) fn selected_item(&self) -> Option<X11DraftItem> {
        let item = self
            .checklist
            .selected()
            .and_then(|index| self.checklist.items.get(index))
            .cloned()?;
        Some(match item {
            X11ChecklistItem::Declaration(line) => X11DraftItem::Declaration(line),
            X11ChecklistItem::Directory(source) => X11DraftItem::Directory(source),
            X11ChecklistItem::Available(source) => X11DraftItem::Available(source),
        })
    }

    pub(super) fn toggle_selected_details(&mut self) {
        self.checklist.toggle_selected_expansion();
    }

    pub(super) fn select_item(&mut self, index: usize) {
        self.checklist.select(index);
    }

    pub(super) fn move_focus(&mut self, action: ScrollAction, runtime_access_available: bool) {
        self.checklist.move_focus(action, runtime_access_available);
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

    pub(super) fn is_expanded(&self, index: usize) -> bool {
        self.checklist.expanded.contains(&index)
    }
}

fn checklist_items(snapshot: &ConfigurationSnapshot) -> Vec<X11ChecklistItem> {
    let has_directory_binding = snapshot.x11_bindings.iter().any(|binding| {
        matches!(
            binding.scope,
            crate::application::configuration::X11BindingScope::Directory
        )
    });
    let mut items = if has_directory_binding {
        Vec::new()
    } else {
        vec![X11ChecklistItem::Directory(PathBuf::from(
            X11_SOCKET_DIRECTORY,
        ))]
    };
    items.extend(
        snapshot
            .x11_bindings
            .iter()
            .map(|binding| X11ChecklistItem::Declaration(binding.line))
            .collect::<Vec<_>>(),
    );
    let configured_sockets = snapshot
        .x11_bindings
        .iter()
        .filter(|binding| {
            matches!(
                binding.scope,
                crate::application::configuration::X11BindingScope::Socket { .. }
            )
        })
        .map(|binding| binding.source.as_path())
        .collect::<BTreeSet<_>>();
    items.extend(
        snapshot
            .host_x11
            .sockets
            .iter()
            .filter(|socket| !configured_sockets.contains(socket.source()))
            .map(|socket| X11ChecklistItem::Available(socket.source().to_path_buf())),
    );
    items
}

pub(super) fn item_toggle_enabled(
    item: &X11ChecklistItem,
    snapshot: &ConfigurationSnapshot,
    draft: &ConfigurationDraft,
) -> bool {
    match item {
        X11ChecklistItem::Declaration(_) => true,
        X11ChecklistItem::Directory(source) => {
            if matches!(
                draft.x11_change_for_source(source),
                Some(crate::application::configuration::X11BindingChange::Add { .. })
            ) {
                true
            } else {
                matches!(
                    draft.x11_projection_scope(snapshot),
                    X11ProjectionScope::None
                )
            }
        }
        X11ChecklistItem::Available(source) => {
            if matches!(
                draft.x11_change_for_source(source),
                Some(crate::application::configuration::X11BindingChange::Add { .. })
            ) {
                true
            } else {
                matches!(
                    draft.x11_projection_scope(snapshot),
                    X11ProjectionScope::None | X11ProjectionScope::Sockets
                )
            }
        }
    }
}
