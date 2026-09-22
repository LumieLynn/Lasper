use std::borrow::Cow;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::navigation::ConfigurationPage;
use super::{
    ConfigurationPane, ConfigurationView, DraftPreviewState, HitAreas, InspectionState, PreviewTab,
    X11ChecklistItem, X11ContentFocus,
};
use crate::application::configuration::{
    ConfigurationCandidateState, ConfigurationPreview, ConfigurationTarget, X11BindRecommendation,
    X11BindingChange, X11BindingDeclaration, X11BindingScope,
};
use crate::application::x11::X11SourceState;
use crate::domain::x11::HostX11Socket;
use crate::tui::views::title_tabs::bordered_title_tab_hitboxes;
use crate::tui::widgets::display::config_text;
use crate::tui::{soft_wrap_text, theme};
use unicode_width::UnicodeWidthStr;

impl ConfigurationView {
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.hits = HitAreas::default();
        frame.render_widget(Clear, area);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
        let kind = match self.target {
            ConfigurationTarget::Machine(_) => "machine",
            ConfigurationTarget::Image(_) => "image",
        };
        let state = if self.saving {
            "Saving"
        } else if !self.draft.is_empty() {
            "MOD"
        } else if matches!(&self.state, InspectionState::Ready(snapshot) if snapshot.document.is_some())
        {
            "SET"
        } else {
            "Inspection"
        };
        frame.render_widget(
            Paragraph::new(format!(
                " Configure: {} | {kind} | {state}",
                self.target.name()
            ))
            .style(
                Style::default()
                    .fg(theme::theme().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            rows[0],
        );

        let content = rows[1];
        if area.width >= 110 {
            let columns = Layout::horizontal([
                Constraint::Length(26),
                Constraint::Percentage(45),
                Constraint::Min(0),
            ])
            .split(content);
            self.hits.navigation = columns[0];
            self.hits.content = columns[1];
            self.hits.preview = columns[2];
        } else if area.width >= 70 && self.pane != ConfigurationPane::Preview {
            let columns =
                Layout::horizontal([Constraint::Length(26), Constraint::Min(0)]).split(content);
            self.hits.navigation = columns[0];
            self.hits.content = columns[1];
        } else {
            match self.pane {
                ConfigurationPane::Navigation => self.hits.navigation = content,
                ConfigurationPane::Content => self.hits.content = content,
                ConfigurationPane::Preview => self.hits.preview = content,
            }
        }
        if self.hits.navigation.width > 0 {
            self.navigation.render(
                frame,
                self.hits.navigation,
                self.pane == ConfigurationPane::Navigation,
            );
        }
        if self.hits.content.width > 0 {
            match self.navigation.active_page() {
                ConfigurationPage::X11 => self.render_content(frame),
            }
        }
        if self.hits.preview.width > 0 {
            self.render_preview(frame);
        }
        let footer = if self.saving {
            Line::from(Span::styled(
                " Saving configuration...",
                Style::default().fg(theme::theme().hint_fg),
            ))
        } else {
            Line::from(vec![
                Span::raw(" "),
                footer_key("r"),
                footer_hint(" Refresh"),
                footer_key("Esc"),
                footer_hint(" Close"),
                footer_key("Tab/⇧Tab"),
                footer_hint(" Pane"),
                footer_key("Space"),
                footer_hint(" Toggle"),
                footer_key("Enter"),
                footer_hint(" Fold"),
                footer_key("c"),
                footer_hint(" Access"),
                footer_key("Ctrl+S"),
                footer_hint(" Save"),
                footer_key("[/]"),
                footer_hint_last(" Tabs"),
            ])
        };
        frame.render_widget(Paragraph::new(footer), rows[2]);
        self.hits.refresh = Rect::new(rows[2].x, rows[2].y, 11.min(rows[2].width), rows[2].height);
        self.hits.close = Rect::new(
            rows[2].x.saturating_add(11),
            rows[2].y,
            11.min(rows[2].width.saturating_sub(11)),
            rows[2].height,
        );
        if self.restart_confirmation.is_some() {
            self.render_restart_confirmation(frame, area);
        } else if self.discard.is_some() {
            self.render_discard_confirmation(frame, area);
        }
        if let Some(dialog) = &mut self.x11_access_dialog {
            dialog.render(frame, area);
        }
    }

    fn block(&self, title: &'static str, pane: ConfigurationPane) -> Block<'static> {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default().fg(crate::tui::widget_border_color(self.pane == pane, true)),
            );
        if title.is_empty() {
            block
        } else {
            block.title(title)
        }
    }

    fn render_content(&mut self, frame: &mut Frame) {
        let block = self.block(" X11 ", ConfigurationPane::Content);
        let inner = block.inner(self.hits.content);
        frame.render_widget(block, self.hits.content);
        let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(inner);
        let message = match &self.state {
            InspectionState::Loading => Some("Loading configuration…"),
            InspectionState::Failed(error) => Some(error.as_str()),
            InspectionState::Ready(snapshot) if snapshot.document.is_none() => Some("No readable configuration found in the inspected locations. See Checks for discovery scope."),
            InspectionState::Ready(_) if self.x11_items.is_empty() => Some("No configured X11 bind or reachable local X11 filesystem endpoint was found."),
            _ => None,
        };
        if let Some(message) = message {
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        } else if let InspectionState::Ready(snapshot) = &self.state {
            let entries = self
                .x11_items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    x11_item_lines(
                        item,
                        snapshot,
                        self.x11_item_change(item),
                        self.x11_item_checked(item),
                        self.expanded.contains(&index),
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
                                Style::default().fg(
                                    if self.x11_content_focus == X11ContentFocus::Bindings {
                                        theme::theme().list_highlight_symbol
                                    } else {
                                        theme::theme().text_secondary
                                    },
                                )
                            } else {
                                Style::default()
                            };
                            ListItem::new(Text::from(lines)).style(style)
                        })
                        .collect::<Vec<_>>(),
                )
                .highlight_symbol(">> ")
                .highlight_style(
                    if self.x11_content_focus == X11ContentFocus::Bindings {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                rows[0],
                &mut self.list,
            );
            let mut y = rows[0].y;
            for (index, height) in heights.iter().enumerate().skip(self.list.offset()) {
                let height = (*height as u16).min(rows[0].bottom().saturating_sub(y));
                if height == 0 {
                    break;
                }
                self.hits
                    .bindings
                    .push((Rect::new(rows[0].x, y, rows[0].width, height), index));
                self.hits
                    .checkboxes
                    .push((Rect::new(rows[0].x, y, 7.min(rows[0].width), 1), index));
                y += height;
            }
        }
        self.render_x11_access_entry(frame, rows[1]);
    }

    fn render_x11_access_entry(&mut self, frame: &mut Frame, area: Rect) {
        let enabled = matches!(self.target, ConfigurationTarget::Machine(_));
        let focused = enabled
            && self.pane == ConfigurationPane::Content
            && self.x11_content_focus == X11ContentFocus::RuntimeAccess;
        self.hits.x11_access = area;
        let label = if enabled {
            " Runtime access... "
        } else {
            " Runtime access is available for running machines "
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
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
                        .border_type(BorderType::Rounded)
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

    fn preview_text(&self) -> Cow<'_, str> {
        match &self.state {
            InspectionState::Loading => Cow::Borrowed("Loading configuration…"),
            InspectionState::Failed(error) => Cow::Borrowed(error),
            InspectionState::Ready(snapshot) => {
                if self.preview_tab == PreviewTab::Diff {
                    let mut text = match &self.draft_preview {
                        DraftPreviewState::Clean => {
                            "No unsaved changes. Check or uncheck an X11 endpoint to generate a diff."
                                .to_owned()
                        }
                        DraftPreviewState::Loading(generation) => {
                            format!("Calculating draft #{generation}...")
                        }
                        DraftPreviewState::Failed {
                            generation,
                            message,
                        } => format!("Draft #{generation} preview failed:\n{message}"),
                        DraftPreviewState::Ready {
                            preview: ConfigurationPreview::Ready { diff, .. },
                            ..
                        } => diff.clone(),
                        DraftPreviewState::Ready {
                            preview: ConfigurationPreview::Unchanged { .. },
                            ..
                        } => "The draft does not change the file bytes.".into(),
                        DraftPreviewState::Ready {
                            preview: ConfigurationPreview::Blocked { reason },
                            ..
                        } => format!("Draft cannot be applied:\n{reason}"),
                        DraftPreviewState::Ready {
                            preview: ConfigurationPreview::Conflict { reason },
                            ..
                        } => format!("Configuration changed:\n{reason}"),
                    };
                    if let Some(error) = &self.apply_error {
                        text.push_str("\n\nSave did not complete:\n");
                        text.push_str(error);
                    }
                    return Cow::Owned(text);
                }
                if self.preview_tab == PreviewTab::Raw {
                    return Cow::Borrowed(
                        snapshot
                            .document
                            .as_ref()
                            .map(|doc| doc.content.as_str())
                            .unwrap_or("No document was read."),
                    );
                }
                let mut lines = vec![
                    "Discovery".to_owned(),
                    snapshot.discovery.description().into(),
                    String::new(),
                ];
                if let Some(document) = &snapshot.document {
                    lines.extend([
                        document.origin.label().into(),
                        document.path.display().to_string(),
                        String::new(),
                        "Document SHA-256".into(),
                        document.content_sha256.clone(),
                        String::new(),
                    ]);
                }
                for candidate in &snapshot.candidates {
                    let state = match &candidate.state {
                        ConfigurationCandidateState::Absent => "Absent",
                        ConfigurationCandidateState::Selected => "Selected",
                        ConfigurationCandidateState::NotConsulted => "Not consulted",
                        ConfigurationCandidateState::Unavailable(_) => "Unavailable",
                    };
                    lines.push(format!("{state}: {}", candidate.path.display()));
                    if let ConfigurationCandidateState::Unavailable(reason) = &candidate.state {
                        lines.push(reason.clone());
                    }
                }
                if let Some(target) = &snapshot.write_target {
                    lines.extend([
                        String::new(),
                        format!(
                            "Administrator target: {} ({})",
                            target.path.display(),
                            if target.exists { "exists" } else { "absent" }
                        ),
                    ]);
                    if snapshot
                        .document
                        .as_ref()
                        .is_some_and(|source| source.path != target.path)
                    {
                        lines.push("The selected source differs from this target. Copying it requires a separate trust and replacement decision.".into());
                    }
                }
                lines.push(String::new());
                lines.extend([format!("{} recognized X11 bind declaration(s)", snapshot.x11_bindings.len()),
                    format!("{} live X11 filesystem endpoint(s)", snapshot.host_x11.sockets.len()),
                    format!("{} other bind declaration(s) in Raw", snapshot.other_bind_count), String::new(),
                    "Declarations are shown without a live socket, mount, guest path or authorization check.".into(),
                    "Alternate endpoint names do not prove display ownership. Directory binds may expose multiple displays.".into(),
                    "Inspecting configuration does not save files, create guest links, or change X11 access.".into()]);
                lines.push(String::new());
                match &snapshot.x11_bind_recommendation {
                    X11BindRecommendation::Ready {
                        private_users,
                        idmapped,
                    } => lines.push(format!(
                        "New endpoint policy: PrivateUsers={private_users}; {}",
                        if *idmapped {
                            "read-only original-path bind with idmap"
                        } else {
                            "read-only original-path bind without idmap"
                        }
                    )),
                    X11BindRecommendation::Unsupported { reason, .. } => {
                        lines.push(format!("New endpoint policy unavailable: {reason}"));
                    }
                }
                for socket in &snapshot.host_x11.sockets {
                    let revision = socket.revision();
                    let (peer_pid, peer_uid, peer_gid) = socket.peer_identity();
                    lines.push(format!(
                        ":{}{} {} -> {} | owner {}:{} mode {:04o} | peer {peer_pid} {peer_uid}:{peer_gid} | dev {} ino {}",
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
                lines.extend(snapshot.host_x11.diagnostics.iter().cloned());
                lines.extend(snapshot.diagnostics.iter().cloned());
                Cow::Owned(lines.join("\n"))
            }
        }
    }

    fn render_preview(&mut self, frame: &mut Frame) {
        let tab_widths = PreviewTab::ALL.map(|tab| (tab, tab.label().width()));
        self.hits.preview_tabs =
            bordered_title_tab_hitboxes(self.hits.preview, Alignment::Left, &tab_widths, 1);
        let t = theme::theme();
        let mut spans = Vec::new();
        for tab in PreviewTab::ALL {
            if !spans.is_empty() {
                spans.push(Span::raw("-"));
            }
            let style = if self.preview_tab == tab {
                Style::default()
                    .fg(if self.pane == ConfigurationPane::Preview {
                        t.tab_active_focused
                    } else {
                        t.tab_active_unfocused
                    })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.tab_inactive)
            };
            spans.push(Span::styled(tab.label(), style));
        }
        let block = self
            .block("", ConfigurationPane::Preview)
            .title(Line::from(spans));
        let inner = block.inner(self.hits.preview);
        frame.render_widget(block, self.hits.preview);
        if self.preview_cache.is_none() || self.preview_width != inner.width {
            let raw = match (&self.state, self.preview_tab) {
                (InspectionState::Ready(snapshot), PreviewTab::Raw) => snapshot.document.as_ref(),
                _ => None,
            };
            self.preview_cache = Some(match raw {
                Some(document) => config_text::wrapped_lines(&document.content, inner.width),
                None if self.preview_tab == PreviewTab::Diff => {
                    diff_lines(&self.preview_text(), inner.width)
                }
                None => soft_wrap_text(&self.preview_text(), inner.width as usize)
                    .into_iter()
                    .map(Line::from)
                    .collect(),
            });
            self.preview_width = inner.width;
        }
        let lines = self.preview_cache.as_ref().expect("preview was built");
        self.preview_max_scroll = lines.len().saturating_sub(inner.height as usize);
        self.preview_scroll = self.preview_scroll.min(self.preview_max_scroll);
        frame.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .skip(self.preview_scroll)
                    .take(inner.height as usize)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            inner,
        );
    }

    fn render_discard_confirmation(&self, frame: &mut Frame, area: Rect) {
        let width = 58.min(area.width);
        let height = 9.min(area.height);
        let dialog = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new(
                "Discard the unsaved X11 configuration draft?\n\n[y] Discard    [n/Esc] Keep editing",
            )
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Unsaved changes ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme::theme().dialog_border_warn)),
            ),
            dialog,
        );
    }

    fn render_restart_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(machine) = &self.restart_confirmation else {
            return;
        };
        let width = 62.min(area.width);
        let height = 9.min(area.height);
        let dialog = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new(format!(
                "Restart {machine} now to activate the saved X11 bind changes?\n\n[y] Restart now    [n/Esc] Later"
            ))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Restart machine? ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme::theme().dialog_border_warn)),
            ),
            dialog,
        );
    }
}

fn footer_key(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(theme::theme().key_hint_fg))
}

fn footer_hint(text: &'static str) -> Span<'static> {
    Span::styled(
        format!("{}, ", text.trim_end()),
        Style::default().fg(theme::theme().hint_fg),
    )
}

fn footer_hint_last(text: &'static str) -> Span<'static> {
    Span::styled(
        text.trim_end().to_owned(),
        Style::default().fg(theme::theme().hint_fg),
    )
}

fn x11_item_lines(
    item: &X11ChecklistItem,
    snapshot: &crate::application::configuration::ConfigurationSnapshot,
    change: Option<&X11BindingChange>,
    checked: bool,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
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
                .map(move |text| Line::styled(text, style))
        })
        .collect()
}

fn declaration_lines(
    bind: &X11BindingDeclaration,
    snapshot: &crate::application::configuration::ConfigurationSnapshot,
    change: Option<&X11BindingChange>,
    check: &str,
    disclosure: &str,
    expanded: bool,
) -> Vec<Line<'static>> {
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
            Some(X11SourceState::Missing) => (
                " [! Missing]",
                "Source path is missing on the host. The configured bind is retained; refresh after changing desktop sessions.",
                Some(theme::theme().error),
            ),
            Some(X11SourceState::Invalid(reason)) => (
                " [! Invalid]", reason.as_str(), Some(theme::theme().error),
            ),
            Some(X11SourceState::Unverified(reason)) => (
                " [! Unverified]", reason.as_str(), Some(theme::theme().warning),
            ),
            _ => (
                " [! Not observed]",
                "No authenticated endpoint was observed. This does not establish that the source is missing. Refresh or inspect Checks for details.",
                Some(theme::theme().warning),
            ),
        }
    };
    let style = color.map_or(Style::default(), |color| Style::default().fg(color));
    let mut lines = vec![Line::styled(
        format!(
            "{check} {disclosure} {label}{badge} [line {}]{modified}",
            bind.line
        ),
        style,
    )];
    if expanded {
        lines.extend([
            Line::from(format!("    Source: {}", source.display())),
            Line::from(format!("    Guest:  {}", target.display())),
            Line::from(format!(
                "    {}{}",
                if readonly { "Read-only" } else { "Read-write" },
                if bind.options.is_empty() {
                    String::new()
                } else {
                    format!("; {}", bind.options.join(","))
                }
            )),
            Line::styled(format!("    Host endpoint: {detail}"), style),
            Line::from(""),
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
) -> Vec<Line<'static>> {
    let current = if socket.alternate() { " alternate" } else { "" };
    let modified = change.map_or("", |_| " [MOD add]");
    let mut lines = vec![Line::from(format!(
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
            Line::from(format!("    Source: {}", socket.source().display())),
            Line::from(format!("    Guest:  {}", socket.source().display())),
            Line::from(format!("    Recommended: {policy}")),
            Line::from(format!(
                "    Socket owner: {}:{} mode {:04o}",
                socket.owner_uid(),
                socket.owner_gid(),
                socket.mode()
            )),
            Line::from(""),
        ]);
    }
    lines
}

fn diff_lines(content: &str, width: u16) -> Vec<Line<'static>> {
    let t = theme::theme();
    content
        .lines()
        .flat_map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default().fg(t.accent)
            } else if line.starts_with('+') {
                Style::default().fg(t.success)
            } else if line.starts_with('-') {
                Style::default().fg(t.error)
            } else if line.starts_with("@@") {
                Style::default().fg(t.warning)
            } else {
                Style::default()
            };
            soft_wrap_text(line, width as usize)
                .into_iter()
                .map(move |wrapped| Line::styled(wrapped, style))
        })
        .collect()
}
