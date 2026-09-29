//! Rendering and line projection for the Wayland configuration page.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Frame;

use super::{WaylandChecklistItem, WaylandPageState};
use crate::application::configuration::{
    recommended_wayland_target, ConfigurationDraft, ConfigurationSnapshot, ConfigurationTarget,
    DisplayBindRecommendation, WaylandBindingChange, WaylandBindingDeclaration, WaylandSourceState,
};
use crate::domain::wayland::HostWaylandSocket;
use crate::tui::configuration::page::{ConfigurationPane, InspectionState, PageInspectionReport};
use crate::tui::configuration::pages::checklist::{
    render_endpoint_checklist, ChecklistBody, ChecklistHitAreas,
};
use crate::tui::{soft_wrap_text, theme};

pub(super) fn inspection_report(snapshot: &ConfigurationSnapshot) -> PageInspectionReport {
    let mut details = Vec::new();
    match &snapshot.wayland_bind_recommendation {
        DisplayBindRecommendation::Ready {
            private_users,
            idmapped,
        } => details.push(format!(
            "New Wayland endpoint policy: PrivateUsers={private_users}; writable managed-path bind{}",
            if *idmapped { " with idmap" } else { "" }
        )),
        DisplayBindRecommendation::Unsupported { reason, .. } => {
            details.push(format!("New Wayland endpoint policy unavailable: {reason}"));
        }
    }
    for socket in &snapshot.host_wayland.sockets {
        let revision = socket.revision();
        let current = snapshot
            .host_wayland
            .preferred_display
            .as_ref()
            .is_some_and(|display| display == socket.display());
        details.push(format!(
            "{}{} {} -> {} | owner {}:{} mode {:04o} | dev {} ino {}",
            socket.display(),
            if current {
                " (current WAYLAND_DISPLAY)"
            } else {
                ""
            },
            socket
                .runtime_dir()
                .join(socket.display().as_str())
                .display(),
            socket.canonical_path().display(),
            socket.owner_uid(),
            socket.owner_gid(),
            socket.mode(),
            revision.device,
            revision.inode,
        ));
    }
    for observation in &snapshot.host_wayland.sources {
        let state = match &observation.state {
            WaylandSourceState::Observed => "observed".to_owned(),
            WaylandSourceState::Missing => "missing".to_owned(),
            WaylandSourceState::Invalid(reason) => format!("invalid: {reason}"),
            WaylandSourceState::Unverified(reason) => format!("unverified: {reason}"),
        };
        details.push(format!(
            "Configured Wayland source {}: {state}",
            observation.source.display()
        ));
    }
    details.extend(snapshot.host_wayland.diagnostics.iter().cloned());
    PageInspectionReport {
        summary: vec![
            format!(
                "{} recognized Wayland bind declaration(s)",
                snapshot.wayland_bindings.len()
            ),
            format!(
                "{} live Wayland socket(s)",
                snapshot.host_wayland.sockets.len()
            ),
        ],
        details,
    }
}

pub(super) fn render(
    page: &mut WaylandPageState,
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
            "No configured or currently available Wayland display was found.",
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
                        page.checklist.expanded.contains(&index),
                        area.width.saturating_sub(5),
                    )
                })
                .collect(),
        ),
    };
    render_endpoint_checklist(
        frame,
        area,
        " Wayland ",
        pane,
        target,
        &mut page.checklist,
        body,
    )
}

fn item_change<'a>(
    item: &WaylandChecklistItem,
    draft: &'a ConfigurationDraft,
) -> Option<&'a WaylandBindingChange> {
    match item {
        WaylandChecklistItem::Declaration(line) => draft.wayland_change_for_declaration(*line),
        WaylandChecklistItem::Available(source) => draft.wayland_change_for_source(source),
    }
}

fn item_checked(item: &WaylandChecklistItem, draft: &ConfigurationDraft) -> bool {
    match item {
        WaylandChecklistItem::Declaration(_) => !matches!(
            item_change(item, draft),
            Some(WaylandBindingChange::Remove { .. })
        ),
        WaylandChecklistItem::Available(_) => matches!(
            item_change(item, draft),
            Some(WaylandBindingChange::Add { .. })
        ),
    }
}

fn item_lines(
    item: &WaylandChecklistItem,
    snapshot: &ConfigurationSnapshot,
    change: Option<&WaylandBindingChange>,
    checked: bool,
    expanded: bool,
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    let disclosure = if expanded { "∨" } else { ">" };
    let check = if checked { "[x]" } else { "[ ]" };
    let lines = match item {
        WaylandChecklistItem::Declaration(line) => {
            let binding = snapshot
                .wayland_bindings
                .iter()
                .find(|binding| binding.line == *line)
                .expect("Wayland checklist declarations come from the snapshot");
            declaration_lines(binding, snapshot, change, check, disclosure, expanded)
        }
        WaylandChecklistItem::Available(source) => {
            let socket = snapshot
                .host_wayland
                .sockets
                .iter()
                .find(|socket| socket.canonical_path() == source)
                .expect("Wayland checklist endpoints come from the host catalog");
            available_lines(socket, snapshot, change, check, disclosure, expanded)
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
    binding: &WaylandBindingDeclaration,
    snapshot: &ConfigurationSnapshot,
    change: Option<&WaylandBindingChange>,
    check: &str,
    disclosure: &str,
    expanded: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let (source, target, readonly) = match change {
        Some(WaylandBindingChange::Update {
            source,
            guest_target,
            readonly,
            ..
        }) => (source, guest_target, *readonly),
        _ => (&binding.source, &binding.guest_target, binding.readonly),
    };
    let current = snapshot
        .host_wayland
        .preferred_display
        .as_ref()
        .is_some_and(|display| display == &binding.display);
    let live =
        snapshot.host_wayland.sockets.iter().any(|socket| {
            socket.canonical_path() == source || socket.display() == &binding.display
        });
    let observation = snapshot
        .host_wayland
        .sources
        .iter()
        .find(|observation| observation.source == *source)
        .map(|observation| &observation.state);
    let (badge, detail, color) = if live {
        ("", "observed now", None)
    } else {
        match observation {
            Some(WaylandSourceState::Missing) => (
                " [! Missing]",
                "Source path is missing on the host. The configured bind is retained.",
                Some(theme::theme().error),
            ),
            Some(WaylandSourceState::Invalid(reason)) => {
                (" [! Invalid]", reason.as_str(), Some(theme::theme().error))
            }
            Some(WaylandSourceState::Unverified(reason)) => (
                " [! Unverified]",
                reason.as_str(),
                Some(theme::theme().warning),
            ),
            _ => (
                " [! Not observed]",
                "No authenticated endpoint was observed for the current desktop session.",
                Some(theme::theme().warning),
            ),
        }
    };
    let modified = match change {
        Some(WaylandBindingChange::Remove { .. }) => " [MOD remove]",
        Some(_) => " [MOD]",
        None => "",
    };
    let current = if current {
        " (current WAYLAND_DISPLAY)"
    } else {
        ""
    };
    let style = color.map_or(Style::default(), |color| Style::default().fg(color));
    let mut lines = vec![ratatui::text::Line::styled(
        format!(
            "{check} {disclosure} {}{current}{badge} [line {}]{modified}",
            binding.display, binding.line
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
                if binding.options.is_empty() {
                    String::new()
                } else {
                    format!("; {}", binding.options.join(","))
                }
            )),
            ratatui::text::Line::styled(format!("    Host endpoint: {detail}"), style),
            ratatui::text::Line::from(""),
        ]);
    }
    lines
}

fn available_lines(
    socket: &HostWaylandSocket,
    snapshot: &ConfigurationSnapshot,
    change: Option<&WaylandBindingChange>,
    check: &str,
    disclosure: &str,
    expanded: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let current = if snapshot
        .host_wayland
        .preferred_display
        .as_ref()
        .is_some_and(|display| display == socket.display())
    {
        " (current WAYLAND_DISPLAY)"
    } else {
        ""
    };
    let modified = change.map_or("", |_| " [MOD add]");
    let mut lines = vec![ratatui::text::Line::from(format!(
        "{check} {disclosure} {}{current} [available]{modified}",
        socket.display(),
    ))];
    if expanded {
        lines.extend([
            ratatui::text::Line::from(format!("    Source: {}", socket.canonical_path().display())),
            ratatui::text::Line::from(format!(
                "    Guest:  {}",
                recommended_wayland_target(socket.owner_uid(), socket.display()).display()
            )),
            ratatui::text::Line::from(format!(
                "    Owner {}:{}; mode {:04o}",
                socket.owner_uid(),
                socket.owner_gid(),
                socket.mode()
            )),
        ]);
        match &snapshot.wayland_bind_recommendation {
            DisplayBindRecommendation::Ready {
                private_users,
                idmapped,
            } => lines.push(ratatui::text::Line::from(format!(
                "    PrivateUsers={private_users}; writable bind{}",
                if *idmapped { " with idmap" } else { "" }
            ))),
            DisplayBindRecommendation::Unsupported { reason, .. } => {
                lines.push(ratatui::text::Line::styled(
                    format!("    Cannot add automatically: {reason}"),
                    Style::default().fg(theme::theme().error),
                ));
            }
        }
        lines.push(ratatui::text::Line::from(""));
    }
    lines
}
