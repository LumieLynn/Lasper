//! X11 configuration page state and local projection.
//!
//! Workspace input, runtime access, and presentation are deliberately kept in
//! sibling modules so extending this page does not grow another monolithic UI
//! implementation.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::checklist::PageChecklist;
use crate::application::configuration::{
    ConfigurationSnapshot, X11BindRecommendation, X11BindingDeclaration,
};
use crate::tui::widgets::dialogs::x11_authorization::X11AuthorizationDialog;

mod controller;
mod effects;
mod presentation;
mod runtime;

pub(crate) use controller::X11PageUpdate;
pub(in crate::tui::configuration) use effects::start_action;
pub(crate) use effects::X11PageEvent;
pub(crate) use runtime::X11PageAction;

pub(in crate::tui::configuration) const PAGE_ID:
    crate::tui::configuration::core::page::ConfigurationPageId =
    crate::tui::configuration::core::page::ConfigurationPageId::X11;

#[derive(Clone, Debug, PartialEq, Eq)]
enum X11ChecklistItem {
    Declaration(usize),
    Available(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum X11DraftItem {
    Declaration(usize),
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
            X11ChecklistItem::Available(source) => X11DraftItem::Available(source),
        })
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

    pub(super) fn is_expanded(&self, index: usize) -> bool {
        self.checklist.expanded.contains(&index)
    }
}

fn checklist_items(snapshot: &ConfigurationSnapshot) -> Vec<X11ChecklistItem> {
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
