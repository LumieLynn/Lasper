//! Rendering and line projection for the X11 configuration page.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Frame;

use super::{X11ChecklistItem, X11PageState};
use crate::application::configuration::{
    ConfigurationDraft, ConfigurationSnapshot, ConfigurationTarget, X11BindRecommendation,
    X11BindingChange, X11BindingDeclaration, X11BindingScope,
};
use crate::domain::x11::HostX11Socket;
use crate::domain::x11::X11PeerIdentity;
use crate::tui::configuration::core::page::PageInspectionReport;
use crate::tui::configuration::core::{ConfigurationPane, InspectionState};
use crate::tui::configuration::pages::checklist::{
    render_endpoint_checklist, ChecklistBody, ChecklistHitAreas,
};
use crate::tui::{soft_wrap_text, theme};

pub(super) fn inspection_report(snapshot: &ConfigurationSnapshot) -> PageInspectionReport {
    let mut details = Vec::new();
    match &snapshot.x11_bind_recommendation {
        X11BindRecommendation::Ready {
            private_users,
            idmapped,
        } => details.push(format!(
            "New endpoint policy: PrivateUsers={private_users}; {}",
            if *idmapped {
                "read-only original-path bind with idmap"
            } else {
                "read-only original-path bind without idmap"
            }
        )),
        X11BindRecommendation::Unsupported { reason, .. } => {
            details.push(format!("New endpoint policy unavailable: {reason}"));
        }
    }
    for socket in &snapshot.host_x11.sockets {
        let revision = socket.revision();
        let peer = match socket.peer_identity() {
            X11PeerIdentity::LocalProcess { pid, uid, gid } => {
                format!("pid {pid} {uid}:{gid}")
            }
            X11PeerIdentity::External { uid, gid } => {
                format!("external {uid}:{gid} (PID unavailable)")
            }
        };
        details.push(format!(
            ":{}{} {} -> {} | owner {}:{} mode {:04o} | peer {peer} | dev {} ino {}",
            socket.display(),
            if socket.alternate() { " alternate" } else { "" },
            socket.source().display(),
            socket.canonical_path().display(),
            socket.owner_uid(),
            socket.owner_gid(),
            socket.mode(),
            revision.device,
            revision.inode,
        ));
    }
    details.extend(snapshot.host_x11.diagnostics.iter().cloned());
    PageInspectionReport {
        summary: vec![
            format!(
                "{} recognized X11 bind declaration(s)",
                snapshot.x11_bindings.len()
            ),
            format!(
                "{} live X11 filesystem endpoint(s)",
                snapshot.host_x11.sockets.len()
            ),
        ],
        details,
    }
}

pub(super) fn render(
    page: &mut X11PageState,
    frame: &mut Frame,
    area: Rect,
    state: &InspectionState,
    target: &ConfigurationTarget,
    pane: ConfigurationPane,
    draft: &ConfigurationDraft,
) -> ChecklistHitAreas {
    let body = match state {
        InspectionState::Loading => ChecklistBody::Message("Loading configuration…"),
        InspectionState::Failed(error) => ChecklistBody::Message(error),
        InspectionState::Ready(snapshot) if snapshot.document.is_none() => ChecklistBody::Message(
            "No readable configuration found in the inspected locations. See Checks for discovery scope.",
        ),
        InspectionState::Ready(_) if page.checklist.items.is_empty() => ChecklistBody::Message(
            "No configured X11 bind or reachable local X11 filesystem endpoint was found.",
        ),
        InspectionState::Ready(snapshot) => ChecklistBody::Entries(
            page.checklist
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    item_lines(
                        item,
                        snapshot,
                        item_change(item, draft),
                        item_checked(item, draft),
                        page.is_expanded(index),
                        area.width.saturating_sub(5),
                    )
                })
                .collect(),
        ),
    };
    render_endpoint_checklist(
        frame,
        area,
        " X11 ",
        pane,
        target,
        &mut page.checklist,
        body,
    )
}

fn item_change<'a>(
    item: &X11ChecklistItem,
    draft: &'a ConfigurationDraft,
) -> Option<&'a X11BindingChange> {
    match item {
        X11ChecklistItem::Declaration(line) => draft.x11_change_for_declaration(*line),
        X11ChecklistItem::Available(source) => draft.x11_change_for_source(source),
    }
}

fn item_checked(item: &X11ChecklistItem, draft: &ConfigurationDraft) -> bool {
    match item {
        X11ChecklistItem::Declaration(_) => !matches!(
            item_change(item, draft),
            Some(X11BindingChange::Remove { .. })
        ),
        X11ChecklistItem::Available(_) => {
            matches!(item_change(item, draft), Some(X11BindingChange::Add { .. }))
        }
    }
}

fn item_lines(
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
