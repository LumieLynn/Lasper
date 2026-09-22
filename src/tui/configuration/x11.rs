use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::{ConfigurationAction, ConfigurationPane, ConfigurationTarget, InspectionState};
use crate::application::configuration::{
    ConfigurationSnapshot, X11BindRecommendation, X11BindingChange, X11BindingDeclaration,
    X11BindingScope,
};
use crate::application::x11::{X11AccessCheck, X11AccessError, X11Authorization};
use crate::domain::x11::HostX11Socket;
use crate::tui::soft_wrap_text;
use crate::tui::theme;
use crate::tui::widgets::dialogs::x11_access::{X11AccessDialog, X11AccessDialogAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum X11ContentFocus {
    Bindings,
    RuntimeAccess,
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

#[derive(Default)]
pub(super) struct X11HitAreas {
    pub(super) bindings: Vec<(Rect, usize)>,
    pub(super) checkboxes: Vec<(Rect, usize)>,
    pub(super) access: Rect,
}

pub(super) struct X11PageState {
    content_focus: X11ContentFocus,
    access_dialog: Option<X11AccessDialog>,
    list: ListState,
    items: Vec<X11ChecklistItem>,
    draft: BTreeMap<X11DraftKey, X11BindingChange>,
    expanded: BTreeSet<usize>,
}

impl Default for X11PageState {
    fn default() -> Self {
        Self {
            content_focus: X11ContentFocus::Bindings,
            access_dialog: None,
            list: ListState::default(),
            items: Vec::new(),
            draft: BTreeMap::new(),
            expanded: BTreeSet::new(),
        }
    }
}

impl X11PageState {
    pub(super) fn begin_query(&mut self) {
        self.access_dialog = None;
        self.items.clear();
        self.list = ListState::default();
        self.content_focus = X11ContentFocus::Bindings;
        self.clear_expanded();
    }

    pub(super) fn finish_query(&mut self, snapshot: &ConfigurationSnapshot) {
        self.items = x11_checklist_items(snapshot);
        self.list = ListState::default().with_selected((!self.items.is_empty()).then_some(0));
        self.clear_expanded();
        self.expanded.insert(0);
        self.content_focus = X11ContentFocus::Bindings;
    }

    pub(super) fn clear_draft(&mut self) {
        self.draft.clear();
    }

    pub(super) fn draft_is_empty(&self) -> bool {
        self.draft.is_empty()
    }

    pub(super) fn draft_changes(&self) -> Vec<X11BindingChange> {
        self.draft.values().cloned().collect()
    }

    pub(super) fn toggle_selected(&mut self) -> bool {
        let Some(item) = self
            .list
            .selected()
            .and_then(|index| self.items.get(index))
            .cloned()
        else {
            return false;
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
        true
    }

    pub(super) fn toggle_selected_details(&mut self) {
        let Some(selected) = self.list.selected() else {
            return;
        };
        if !self.expanded.remove(&selected) {
            self.expanded.insert(selected);
        }
    }

    pub(super) fn select_item(&mut self, index: usize) {
        if self.list.selected() != Some(index) {
            self.list.select(Some(index));
        }
    }

    pub(super) fn move_focus(&mut self, down: bool, runtime_access_available: bool) {
        match (self.content_focus, down) {
            (X11ContentFocus::Bindings, true) => {
                let current = self.list.selected().unwrap_or(0);
                if current + 1 < self.items.len() {
                    self.select_item(current + 1);
                } else if runtime_access_available {
                    self.content_focus = X11ContentFocus::RuntimeAccess;
                }
            }
            (X11ContentFocus::Bindings, false) => {
                let current = self.list.selected().unwrap_or(0);
                self.select_item(current.saturating_sub(1));
            }
            (X11ContentFocus::RuntimeAccess, false) if !self.items.is_empty() => {
                self.select_item(self.items.len() - 1);
                self.content_focus = X11ContentFocus::Bindings;
            }
            _ => {}
        }
    }

    pub(super) fn is_bindings_focused(&self) -> bool {
        self.content_focus == X11ContentFocus::Bindings
    }

    pub(super) fn is_runtime_access_focused(&self) -> bool {
        self.content_focus == X11ContentFocus::RuntimeAccess
    }

    pub(super) fn set_bindings_focus(&mut self) {
        self.content_focus = X11ContentFocus::Bindings;
    }

    pub(super) fn set_runtime_access_focus(&mut self) {
        self.content_focus = X11ContentFocus::RuntimeAccess;
    }

    pub(super) fn set_expanded(&mut self, index: usize, expanded: bool) {
        if expanded {
            self.expanded.insert(index);
        } else {
            self.expanded.remove(&index);
        }
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.list.selected()
    }

    pub(super) fn is_expanded(&self, index: usize) -> bool {
        self.expanded.contains(&index)
    }

    pub(super) fn clear_expanded(&mut self) {
        self.expanded.clear();
    }

    pub(super) fn access_dialog_is_open(&self) -> bool {
        self.access_dialog.is_some()
    }

    pub(super) fn handle_access_key(&mut self, key: KeyEvent) -> Option<ConfigurationAction> {
        let action = {
            let dialog = self.access_dialog.as_mut()?;
            dialog.handle_key(key)
        };
        Some(self.handle_access_action(action))
    }

    pub(super) fn selected_host_socket(
        &self,
        snapshot: &ConfigurationSnapshot,
    ) -> Option<HostX11Socket> {
        let item = self.list.selected().and_then(|index| self.items.get(index));
        match item {
            Some(X11ChecklistItem::Available(source)) => snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| socket.source() == source)
                .cloned(),
            Some(X11ChecklistItem::Declaration(line)) => {
                let binding = snapshot
                    .x11_bindings
                    .iter()
                    .find(|binding| binding.line == *line)?;
                match binding.scope {
                    X11BindingScope::Socket { .. } => snapshot
                        .host_x11
                        .sockets
                        .iter()
                        .find(|socket| socket.source() == binding.source)
                        .cloned(),
                    X11BindingScope::Directory => preferred_standard_socket(snapshot),
                }
            }
            None => preferred_standard_socket(snapshot),
        }
    }

    pub(super) fn open_access_dialog(
        &mut self,
        target: &ConfigurationTarget,
        snapshot: &ConfigurationSnapshot,
    ) {
        let ConfigurationTarget::Machine(machine) = target else {
            return;
        };
        let selected = self.selected_host_socket(snapshot);
        self.access_dialog = Some(X11AccessDialog::new(
            machine.clone(),
            snapshot.host_x11.sockets.clone(),
            selected.as_ref(),
        ));
    }

    pub(super) fn handle_access_action(
        &mut self,
        action: X11AccessDialogAction,
    ) -> ConfigurationAction {
        match action {
            X11AccessDialogAction::None => ConfigurationAction::None,
            X11AccessDialogAction::Close => {
                self.access_dialog = None;
                ConfigurationAction::None
            }
            X11AccessDialogAction::Check {
                generation,
                target,
                host_socket,
            } => ConfigurationAction::CheckX11 {
                generation,
                target,
                host_socket,
            },
            X11AccessDialogAction::Authorize {
                generation,
                target,
                host_socket,
            } => ConfigurationAction::AuthorizeX11 {
                generation,
                target,
                host_socket,
            },
            X11AccessDialogAction::Revoke {
                generation,
                target,
                host_socket,
                record_id,
            } => ConfigurationAction::RevokeX11 {
                generation,
                target,
                host_socket,
                record_id,
            },
        }
    }

    pub(super) fn render_access_dialog(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.render(frame, area);
        }
    }

    pub(super) fn track_check(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_check(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn finish_check(
        &mut self,
        generation: u64,
        target: &ConfigurationTarget,
        current_target: &ConfigurationTarget,
        result: Result<X11AccessCheck, X11AccessError>,
    ) {
        if target != current_target {
            return;
        }
        if let Some(dialog) = &mut self.access_dialog {
            dialog.finish_check(generation, result);
        }
    }

    pub(super) fn track_authorization(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_authorization(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn finish_authorization(
        &mut self,
        generation: u64,
        target: &ConfigurationTarget,
        current_target: &ConfigurationTarget,
        result: Result<X11Authorization, X11AccessError>,
    ) {
        if target != current_target {
            return;
        }
        if let Some(dialog) = &mut self.access_dialog {
            dialog.finish_authorization(generation, result);
        }
    }

    pub(super) fn track_revocation(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(dialog) = &mut self.access_dialog {
            dialog.track_revocation(task);
        } else {
            task.abort();
        }
    }

    pub(super) fn finish_revocation(
        &mut self,
        generation: u64,
        target: &ConfigurationTarget,
        current_target: &ConfigurationTarget,
        result: Result<crate::application::x11::X11Revocation, X11AccessError>,
    ) {
        if target != current_target {
            return;
        }
        if let Some(dialog) = &mut self.access_dialog {
            dialog.finish_revocation(generation, result);
        }
    }

    pub(super) fn render_content(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        state: &InspectionState,
        target: &ConfigurationTarget,
        pane: ConfigurationPane,
    ) -> X11HitAreas {
        let mut hits = X11HitAreas::default();
        let block = Block::default()
            .title(" X11 ")
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(crate::tui::widget_border_color(
                pane == ConfigurationPane::Content,
                true,
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Min(0),
            ratatui::layout::Constraint::Length(3),
        ])
        .split(inner);
        let message = match state {
            InspectionState::Loading => Some("Loading configuration…"),
            InspectionState::Failed(error) => Some(error.as_str()),
            InspectionState::Ready(snapshot) if snapshot.document.is_none() => Some(
                "No readable configuration found in the inspected locations. See Checks for discovery scope.",
            ),
            InspectionState::Ready(_) if self.items.is_empty() => Some(
                "No configured X11 bind or reachable local X11 filesystem endpoint was found.",
            ),
            _ => None,
        };
        if let Some(message) = message {
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        } else if let InspectionState::Ready(snapshot) = state {
            let entries = self
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    x11_item_lines(
                        item,
                        snapshot,
                        self.item_change(item),
                        self.item_checked(item),
                        self.is_expanded(index),
                        rows[0].width.saturating_sub(3),
                    )
                })
                .collect::<Vec<_>>();
            let heights = entries.iter().map(Vec::len).collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, lines)| {
                            let style = if self.list.selected() == Some(index) {
                                Style::default().fg(if self.is_bindings_focused() {
                                    theme::theme().list_highlight_symbol
                                } else {
                                    theme::theme().text_secondary
                                })
                            } else {
                                Style::default()
                            };
                            ListItem::new(Text::from(lines)).style(style)
                        })
                        .collect::<Vec<_>>(),
                )
                .highlight_symbol(">> ")
                .highlight_style(if self.is_bindings_focused() {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
                rows[0],
                &mut self.list,
            );
            let mut y = rows[0].y;
            for (index, height) in heights.iter().enumerate().skip(self.list.offset()) {
                let height = (*height as u16).min(rows[0].bottom().saturating_sub(y));
                if height == 0 {
                    break;
                }
                hits.bindings
                    .push((Rect::new(rows[0].x, y, rows[0].width, height), index));
                hits.checkboxes
                    .push((Rect::new(rows[0].x, y, 7.min(rows[0].width), 1), index));
                y += height;
            }
        }
        self.render_access_entry(frame, rows[1], target, pane, &mut hits);
        hits
    }

    fn render_access_entry(
        &self,
        frame: &mut Frame,
        area: Rect,
        target: &ConfigurationTarget,
        pane: ConfigurationPane,
        hits: &mut X11HitAreas,
    ) {
        let enabled = matches!(target, ConfigurationTarget::Machine(_));
        let focused =
            enabled && pane == ConfigurationPane::Content && self.is_runtime_access_focused();
        hits.access = area;
        let label = if enabled {
            " Runtime access... "
        } else {
            " Runtime access is available for running machines "
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(ratatui::layout::Alignment::Center)
                .style(if !enabled {
                    Style::default().fg(theme::theme().text_dim)
                } else if focused {
                    Style::default()
                        .fg(theme::theme().button_focused_fg)
                        .bg(theme::theme().button_focused_bg)
                } else {
                    Style::default().fg(theme::theme().button_unfocused_fg)
                })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(Style::default().fg(if !enabled {
                            theme::theme().border_disabled
                        } else if focused {
                            theme::theme().button_border_focused
                        } else {
                            theme::theme().button_border_unfocused
                        })),
                ),
            area,
        );
    }

    fn item_change(&self, item: &X11ChecklistItem) -> Option<&X11BindingChange> {
        let key = match item {
            X11ChecklistItem::Declaration(line) => X11DraftKey::Declaration(*line),
            X11ChecklistItem::Available(source) => X11DraftKey::Addition(source.clone()),
        };
        self.draft.get(&key)
    }

    fn item_checked(&self, item: &X11ChecklistItem) -> bool {
        match item {
            X11ChecklistItem::Declaration(_) => !matches!(
                self.item_change(item),
                Some(X11BindingChange::Remove { .. })
            ),
            X11ChecklistItem::Available(_) => {
                matches!(self.item_change(item), Some(X11BindingChange::Add { .. }))
            }
        }
    }
}

fn preferred_standard_socket(snapshot: &ConfigurationSnapshot) -> Option<HostX11Socket> {
    snapshot
        .host_x11
        .preferred_display
        .and_then(|display| {
            snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| socket.display() == display && !socket.alternate())
        })
        .or_else(|| {
            snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| !socket.alternate())
        })
        .cloned()
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

fn x11_item_lines(
    item: &X11ChecklistItem,
    snapshot: &ConfigurationSnapshot,
    change: Option<&X11BindingChange>,
    checked: bool,
    expanded: bool,
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    let disclosure = if expanded { "∨" } else { ">" };
    let check = if checked { "[x]" } else { "[ ]" };
    let lines = match item {
        X11ChecklistItem::Declaration(line) => {
            let bind = snapshot
                .x11_bindings
                .iter()
                .find(|binding| binding.line == *line)
                .expect("checklist declarations come from the current snapshot");
            declaration_lines(bind, snapshot, change, check, disclosure, expanded)
        }
        X11ChecklistItem::Available(source) => {
            let socket = snapshot
                .host_x11
                .sockets
                .iter()
                .find(|socket| socket.source() == source)
                .expect("checklist endpoints come from the current host catalog");
            available_socket_lines(
                socket,
                &snapshot.x11_bind_recommendation,
                change,
                check,
                disclosure,
                expanded,
            )
        }
    };
    lines
        .into_iter()
        .flat_map(|line| {
            let style = line.style;
            soft_wrap_text(&line.to_string(), width as usize)
                .into_iter()
                .map(move |text| ratatui::text::Line::styled(text, style))
        })
        .collect()
}

fn declaration_lines(
    bind: &X11BindingDeclaration,
    snapshot: &ConfigurationSnapshot,
    change: Option<&X11BindingChange>,
    check: &str,
    disclosure: &str,
    expanded: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let (source, target, readonly) = match change {
        Some(X11BindingChange::Update {
            source,
            guest_target,
            readonly,
            ..
        }) => (source, guest_target, *readonly),
        _ => (&bind.source, &bind.guest_target, bind.readonly),
    };
    let label = match bind.scope {
        X11BindingScope::Directory => "Socket directory (all displays)".to_owned(),
        X11BindingScope::Socket { display, alternate } => format!(
            ":{display} {}{}",
            source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("endpoint"),
            if alternate { " (alternate)" } else { "" },
        ),
    };
    let modified = match change {
        Some(X11BindingChange::Remove { .. }) => " [MOD remove]",
        Some(_) => " [MOD]",
        None => "",
    };
    let live = snapshot
        .host_x11
        .sockets
        .iter()
        .any(|socket| match bind.scope {
            X11BindingScope::Directory => socket.source().parent() == Some(source.as_path()),
            X11BindingScope::Socket { .. } => socket.source() == source,
        });
    let state = snapshot
        .host_x11
        .sources
        .iter()
        .find(|observation| observation.source == *source)
        .map(|observation| &observation.state);
    let (badge, detail, color) = if live {
        ("", "observed now", None)
    } else {
        match state {
            Some(crate::application::x11::X11SourceState::Missing) => (
                " [! Missing]",
                "Source path is missing on the host. The configured bind is retained; refresh after changing desktop sessions.",
                Some(theme::theme().error),
            ),
            Some(crate::application::x11::X11SourceState::Invalid(reason)) => (
                " [! Invalid]",
                reason.as_str(),
                Some(theme::theme().error),
            ),
            Some(crate::application::x11::X11SourceState::Unverified(reason)) => (
                " [! Unverified]",
                reason.as_str(),
                Some(theme::theme().warning),
            ),
            _ => (
                " [! Not observed]",
                "No authenticated endpoint was observed. This does not establish that the source is missing. Refresh or inspect Checks for details.",
                Some(theme::theme().warning),
            ),
        }
    };
    let style = color.map_or(Style::default(), |color| Style::default().fg(color));
    let mut lines = vec![ratatui::text::Line::styled(
        format!(
            "{check} {disclosure} {label}{badge} [line {}]{modified}",
            bind.line
        ),
        style,
    )];
    if expanded {
        lines.extend([
            ratatui::text::Line::from(format!("    Source: {}", source.display())),
            ratatui::text::Line::from(format!("    Guest:  {}", target.display())),
            ratatui::text::Line::from(format!(
                "    {}{}",
                if readonly { "Read-only" } else { "Read-write" },
                if bind.options.is_empty() {
                    String::new()
                } else {
                    format!("; {}", bind.options.join(","))
                }
            )),
            ratatui::text::Line::styled(format!("    Host endpoint: {detail}"), style),
            ratatui::text::Line::from(""),
        ]);
    }
    lines
}

fn available_socket_lines(
    socket: &HostX11Socket,
    recommendation: &X11BindRecommendation,
    change: Option<&X11BindingChange>,
    check: &str,
    disclosure: &str,
    expanded: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let current = if socket.alternate() { " alternate" } else { "" };
    let modified = change.map_or("", |_| " [MOD add]");
    let mut lines = vec![ratatui::text::Line::from(format!(
        "{check} {disclosure} :{} {}{current} [available]{modified}",
        socket.display(),
        socket
            .source()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("endpoint"),
    ))];
    if expanded {
        let policy = match recommendation {
            X11BindRecommendation::Ready { idmapped, .. } => format!(
                "Read-only original-path bind{}",
                if *idmapped { " with idmap" } else { "" }
            ),
            X11BindRecommendation::Unsupported { reason, .. } => {
                format!("Cannot add automatically: {reason}")
            }
        };
        lines.extend([
            ratatui::text::Line::from(format!("    Source: {}", socket.source().display())),
            ratatui::text::Line::from(format!("    Guest:  {}", socket.source().display())),
            ratatui::text::Line::from(format!("    Recommended: {policy}")),
            ratatui::text::Line::from(format!(
                "    Socket owner: {}:{} mode {:04o}",
                socket.owner_uid(),
                socket.owner_gid(),
                socket.mode()
            )),
            ratatui::text::Line::from(""),
        ]);
    }
    lines
}
