use std::borrow::Cow;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use super::{
    ConfigurationPane, ConfigurationView, DraftPreviewState, HitAreas, InspectionState, PreviewTab,
};
use crate::application::configuration::{
    ConfigurationCandidateState, ConfigurationPreview, ConfigurationTarget,
};
use crate::tui::features::configuration::page::PageRenderContext;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::widgets::display::config_text;
use crate::tui::foundation::widgets::display::title_tabs::bordered_title_tab_hitboxes;
use crate::tui::foundation::{text::soft_wrap_text, theme};
use crate::tui::protocol::Component;
use unicode_width::UnicodeWidthStr;

impl ConfigurationView {
    #[cfg(test)]
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_layer(frame, area, true);
    }

    pub(crate) fn render_layer(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        self.hits = HitAreas::default();
        self.preview_scrollbar.clear_mouse_area();
        frame.render_widget(Clear, area);
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
        let kind = match self.target {
            ConfigurationTarget::Machine(_) => "machine",
            ConfigurationTarget::Image(_) => "image",
        };
        let state = if self.saving {
            "Saving"
        } else if !self.draft_is_empty() {
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
            self.pages.render(
                self.navigation.current_page(),
                frame,
                self.hits.content,
                PageRenderContext {
                    state: &self.state,
                    target: &self.target,
                    pane: self.pane,
                    draft: &self.draft,
                },
            );
        }
        if self.hits.preview.width > 0 {
            self.render_preview(frame);
        }
        if self.restart_confirmation.is_some() {
            self.render_restart_confirmation(frame, area);
        } else if self.discard.is_some() {
            self.render_discard_confirmation(frame, area);
        }
        self.pages
            .render_overlay(self.navigation.current_page(), frame, area, foreground);
    }

    fn block(&self, title: &'static str, pane: ConfigurationPane) -> Block<'static> {
        let block =
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(
                    crate::tui::foundation::theme::widget_border_color(self.pane == pane, true),
                ));
        if title.is_empty() {
            block
        } else {
            block.title(title)
        }
    }

    fn preview_text(&self) -> Cow<'_, str> {
        match &self.state {
            InspectionState::Loading => Cow::Borrowed("Loading configuration…"),
            InspectionState::Failed(error) => Cow::Borrowed(error),
            InspectionState::Ready(snapshot) => {
                if self.preview_tab == PreviewTab::Diff {
                    let mut text = match &self.draft_preview {
                        DraftPreviewState::Clean => {
                            "No unsaved changes. Check or uncheck a display endpoint to generate a diff."
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
                let page_report = self.pages.inspection_report(snapshot);
                lines.push(String::new());
                lines.extend(page_report.summary);
                lines.extend([
                    format!("{} other bind declaration(s) in Raw", snapshot.other_bind_count),
                    String::new(),
                    "Declarations are shown without a live socket, mount, guest path or authorization check.".into(),
                    "Alternate endpoint names do not prove display ownership. Directory binds may expose multiple displays.".into(),
                    "Inspecting configuration does not save files, create guest links, or change runtime access.".into(),
                    String::new(),
                ]);
                lines.extend(page_report.details);
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
        self.preview_scroll
            .update(lines.len(), usize::from(inner.height));
        frame.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .skip(self.preview_scroll.offset())
                    .take(inner.height as usize)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            inner,
        );
        self.preview_scrollbar.render(
            frame,
            Rect::new(inner.right(), inner.y, 1, inner.height),
            &self.preview_scroll,
        );
    }

    fn render_discard_confirmation(&self, frame: &mut Frame, area: Rect) {
        ConfirmationDialog::new(
            "Unsaved changes",
            "Discard the unsaved configuration draft?",
        )
        .with_hints(&[("y", "discard"), ("n/Esc", "keep editing")])
        .render(frame, area);
    }

    fn render_restart_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(machine) = &self.restart_confirmation else {
            return;
        };
        ConfirmationDialog::new(
            "Restart machine?",
            format!("Restart {machine} now to activate the saved display bind changes?"),
        )
        .with_hints(&[("y", "restart now"), ("n/Esc", "later")])
        .render(frame, area);
    }
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
