use std::borrow::Cow;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::{ConfigurationPane, ConfigurationView, HitAreas, InspectionState, PreviewTab};
use crate::application::configuration::{
    ConfigurationTarget, X11BindingDeclaration, X11BindingScope,
};
use crate::tui::{soft_wrap_text, theme};

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
        frame.render_widget(
            Paragraph::new(format!(
                " Configure: {} | {kind} | Inspection",
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
                Constraint::Length(22),
                Constraint::Percentage(45),
                Constraint::Min(0),
            ])
            .split(content);
            self.hits.navigation = columns[0];
            self.hits.content = columns[1];
            self.hits.preview = columns[2];
        } else if area.width >= 70 && self.pane != ConfigurationPane::Preview {
            let columns =
                Layout::horizontal([Constraint::Length(22), Constraint::Min(0)]).split(content);
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
            frame.render_widget(
                Paragraph::new("Host Integration\n  X11 (view)")
                    .block(self.block(" Sections ", ConfigurationPane::Navigation)),
                self.hits.navigation,
            );
        }
        if self.hits.content.width > 0 {
            self.render_content(frame);
        }
        if self.hits.preview.width > 0 {
            self.render_preview(frame);
        }
        frame.render_widget(
            Paragraph::new(" r Refresh  Esc Close  Tab/⇧Tab Pane  Enter Expand  [/] Checks/Raw")
                .style(Style::default().fg(theme::theme().hint_fg)),
            rows[2],
        );
        self.hits.refresh = Rect::new(rows[2].x, rows[2].y, 11.min(rows[2].width), rows[2].height);
        self.hits.close = Rect::new(
            rows[2].x.saturating_add(11),
            rows[2].y,
            11.min(rows[2].width.saturating_sub(11)),
            rows[2].height,
        );
    }

    fn block(&self, title: &'static str, pane: ConfigurationPane) -> Block<'static> {
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default().fg(crate::tui::widget_border_color(self.pane == pane, true)),
            )
    }

    fn render_content(&mut self, frame: &mut Frame) {
        let block = self.block(" X11 ", ConfigurationPane::Content);
        let inner = block.inner(self.hits.content);
        frame.render_widget(block, self.hits.content);
        let rows = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(inner);
        let message = match &self.state {
            InspectionState::Loading => Some("Loading configuration…"),
            InspectionState::Failed(error) => Some(error.as_str()),
            InspectionState::Ready(snapshot) if snapshot.document.is_none() => Some("No readable configuration found in the inspected locations. See Checks for discovery scope."),
            InspectionState::Ready(snapshot) if snapshot.x11_bindings.is_empty() => Some("No standard host X11 source declaration found. Custom sources and other bindings remain available in Raw."),
            _ => None,
        };
        if let Some(message) = message {
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        } else if let InspectionState::Ready(snapshot) = &self.state {
            let entries = snapshot
                .x11_bindings
                .iter()
                .enumerate()
                .map(|(index, bind)| {
                    binding_lines(
                        bind,
                        self.expanded.contains(&index),
                        rows[0].width.saturating_sub(2),
                    )
                })
                .collect::<Vec<_>>();
            let heights = entries.iter().map(Vec::len).collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(
                    entries
                        .into_iter()
                        .map(|lines| ListItem::new(Text::from(lines)))
                        .collect::<Vec<_>>(),
                )
                .highlight_symbol("› ")
                .highlight_style(Style::default().fg(theme::theme().accent)),
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
                y += height;
            }
        }
        frame.render_widget(
            Paragraph::new(
                "Not queried. A startup bind does not establish current X server access.",
            )
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Current access ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            ),
            rows[1],
        );
        frame.render_widget(
            Paragraph::new("Operation history: not loaded")
                .style(Style::default().fg(theme::theme().text_secondary)),
            rows[2],
        );
    }

    fn preview_text(&self) -> Cow<'_, str> {
        match &self.state {
            InspectionState::Loading => Cow::Borrowed("Loading configuration…"),
            InspectionState::Failed(error) => Cow::Borrowed(error),
            InspectionState::Ready(snapshot) => {
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
                lines.extend([format!("{} recognized X11 bind declaration(s)", snapshot.x11_bindings.len()),
                    format!("{} other bind declaration(s) in Raw", snapshot.other_bind_count), String::new(),
                    "Declarations are shown without a live socket, mount, guest path or authorization check.".into(),
                    "Alternate endpoint names do not prove display ownership. Directory binds may expose multiple displays.".into(),
                    "Inspecting configuration does not save files, create guest links, or change X11 access.".into()]);
                lines.extend(snapshot.diagnostics.iter().cloned());
                Cow::Owned(lines.join("\n"))
            }
        }
    }

    fn render_preview(&mut self, frame: &mut Frame) {
        let block = self.block(" Source / Checks ", ConfigurationPane::Preview);
        let inner = block.inner(self.hits.preview);
        frame.render_widget(block, self.hits.preview);
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        let tab_style = |tab| {
            if self.preview_tab == tab {
                Style::default()
                    .fg(theme::theme().accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::theme().text_secondary)
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Checks  ", tab_style(PreviewTab::Checks)),
                Span::styled("Raw ", tab_style(PreviewTab::Raw)),
            ])),
            rows[0],
        );
        self.hits.checks_tab =
            Rect::new(rows[0].x, rows[0].y, 9.min(rows[0].width), rows[0].height);
        self.hits.raw_tab = Rect::new(
            rows[0].x.saturating_add(9),
            rows[0].y,
            4.min(rows[0].width.saturating_sub(9)),
            rows[0].height,
        );
        if self.preview_cache.is_none() || self.preview_width != rows[1].width {
            self.preview_cache = Some(soft_wrap_text(&self.preview_text(), rows[1].width as usize));
            self.preview_width = rows[1].width;
        }
        let lines = self.preview_cache.as_ref().expect("preview was built");
        self.preview_max_scroll = lines.len().saturating_sub(rows[1].height as usize);
        self.preview_scroll = self.preview_scroll.min(self.preview_max_scroll);
        frame.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .skip(self.preview_scroll)
                    .take(rows[1].height as usize)
                    .map(|line| Line::from(line.as_str()))
                    .collect::<Vec<_>>(),
            ),
            rows[1],
        );
    }
}

fn binding_lines(bind: &X11BindingDeclaration, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let label = match bind.scope {
        X11BindingScope::Directory => "Socket directory (multiple displays)".to_string(),
        X11BindingScope::Socket { display, alternate } => format!(
            ":{display}{}",
            if alternate {
                " (alternate endpoint candidate)"
            } else {
                " (declared endpoint)"
            }
        ),
    };
    let mut lines = vec![Line::from(format!(
        "{} {label} [line {}]",
        if expanded { "▾" } else { "▸" },
        bind.line
    ))];
    if expanded {
        lines.extend([
            Line::from(format!("  Source: {}", bind.source.display())),
            Line::from(format!("  Guest:  {}", bind.guest_target.display())),
            Line::from(format!(
                "  {}{}",
                if bind.readonly {
                    "Read-only"
                } else {
                    "Read-write"
                },
                if bind.options.is_empty() {
                    String::new()
                } else {
                    format!("; {}", bind.options.join(","))
                }
            )),
            Line::from("  Client path: not verified"),
            Line::from(""),
        ]);
    }
    lines
        .into_iter()
        .flat_map(|line| {
            soft_wrap_text(&line.to_string(), width as usize)
                .into_iter()
                .map(Line::from)
        })
        .collect()
}
