//! The preview composes one borrowed text control. Layout, input and display
//! use that control; content is derived lazily from the current inspection and
//! draft when its cached rows are invalidated.

use std::borrow::Cow;

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::{ConfigurationPane, ConfigurationView, DraftPreviewState, InspectionState};
use crate::application::configuration::{ConfigurationCandidateState, ConfigurationPreview};
use crate::tui::features::configuration::pages::ConfigurationPages;
use crate::tui::foundation::components::text_block::{TextBlockControl, TextBlockState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::control::{Control, ControlContext};
use crate::tui::foundation::widgets::display::config_text;
use crate::tui::foundation::widgets::display::text::{
    TextContent, TextLayout, TextLayoutCache, TextRows,
};
use crate::tui::foundation::widgets::display::title_tabs::{
    bordered_title_tab_hitboxes, clicked_title_tab, TitleTabHitbox,
};
use crate::tui::foundation::{text::soft_wrap_text, theme};
use crossterm::event::MouseEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PreviewTab {
    Diff,
    Checks,
    Raw,
}

impl PreviewTab {
    const ALL: [Self; 3] = [Self::Diff, Self::Raw, Self::Checks];

    fn label(self) -> &'static str {
        match self {
            Self::Diff => " Diff ",
            Self::Raw => " Raw ",
            Self::Checks => " Checks ",
        }
    }

    pub(super) fn adjacent(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("preview tab is listed");
        Self::ALL[(index + if forward { 1 } else { Self::ALL.len() - 1 }) % Self::ALL.len()]
    }
}

impl ConfigurationView {
    pub(super) fn with_preview<R>(
        &mut self,
        visit: impl FnOnce(TextBlockControl<'_, PreviewContent<'_>>) -> R,
    ) -> R {
        let source = PreviewSource {
            state: &self.state,
            draft_preview: &self.draft_preview,
            apply_error: self.apply_error.as_deref(),
            pages: &self.pages,
        };
        self.preview.with_control(source, visit)
    }

    pub(super) fn render_preview(&mut self, frame: &mut Frame) {
        let area = self.hits.preview;
        let tab_widths = PreviewTab::ALL.map(|tab| (tab, tab.label().width()));
        self.preview.tab_hits = bordered_title_tab_hitboxes(area, Alignment::Left, &tab_widths, 1);
        let context = ControlContext {
            focused: self.pane == ConfigurationPane::Preview,
            enabled: true,
        };
        let block = self.preview.block(context.focused);
        self.with_preview(|control| control.with_block(block).render(frame, area, context));
    }
}

pub(super) struct PreviewPane {
    tab: PreviewTab,
    view: TextBlockState,
    cache: TextLayoutCache,
    hints: &'static [FooterHint],
    tab_hits: Vec<TitleTabHitbox<PreviewTab>>,
}

impl Default for PreviewPane {
    fn default() -> Self {
        Self {
            tab: PreviewTab::Checks,
            view: TextBlockState::default(),
            cache: TextLayoutCache::default(),
            hints: &[],
            tab_hits: Vec::new(),
        }
    }
}

impl PreviewPane {
    pub(super) fn tab(&self) -> PreviewTab {
        self.tab
    }

    pub(super) fn select_tab(&mut self, tab: PreviewTab) {
        if self.tab != tab {
            self.tab = tab;
            self.reset();
        }
    }

    pub(super) fn reset(&mut self) {
        self.view.scroll.reset();
        self.invalidate();
    }

    pub(super) fn invalidate(&mut self) {
        self.cache.invalidate();
        self.view.clear_mouse_area();
    }

    pub(super) fn clear_mouse_area(&mut self) {
        self.view.clear_mouse_area();
        self.tab_hits.clear();
    }

    pub(super) fn clicked_tab(&self, mouse: MouseEvent) -> Option<PreviewTab> {
        clicked_title_tab(&self.tab_hits, mouse)
    }

    pub(super) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        for hint in self.hints {
            footer.register(*hint);
        }
    }

    #[cfg(test)]
    pub(super) fn scroll(&self) -> crate::tui::foundation::scrolling::ScrollState {
        self.view.scroll
    }

    #[cfg(test)]
    pub(super) fn tab_hits(&self) -> &[TitleTabHitbox<PreviewTab>] {
        &self.tab_hits
    }

    fn block(&self, focused: bool) -> Block<'static> {
        let t = theme::theme();
        let mut spans = Vec::new();
        for tab in PreviewTab::ALL {
            if !spans.is_empty() {
                spans.push(Span::raw("-"));
            }
            let style = if self.tab == tab {
                Style::default()
                    .fg(if focused {
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
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Line::from(spans))
    }

    fn with_control<R>(
        &mut self,
        source: PreviewSource<'_>,
        visit: impl FnOnce(TextBlockControl<'_, PreviewContent<'_>>) -> R,
    ) -> R {
        let content = PreviewContent {
            source,
            tab: self.tab,
            cache: &mut self.cache,
        };
        let control = TextBlockControl::new(&mut self.view, "", content);
        self.hints = control.hints();
        visit(control)
    }
}

struct PreviewSource<'a> {
    state: &'a InspectionState,
    draft_preview: &'a DraftPreviewState,
    apply_error: Option<&'a str>,
    pages: &'a ConfigurationPages,
}

pub(super) struct PreviewContent<'a> {
    source: PreviewSource<'a>,
    tab: PreviewTab,
    cache: &'a mut TextLayoutCache,
}

impl TextContent for PreviewContent<'_> {
    fn layout(&mut self, width: u16, top_row: usize) -> TextLayout {
        if self.cache.needs_layout(width) {
            let raw = match (self.source.state, self.tab) {
                (InspectionState::Ready(snapshot), PreviewTab::Raw) => snapshot.document.as_ref(),
                _ => None,
            };
            let lines = match raw {
                Some(document) => config_text::wrapped_lines(&document.content, width),
                None if self.tab == PreviewTab::Diff => diff_lines(&self.text(), width),
                None => soft_wrap_text(&self.text(), usize::from(width))
                    .into_iter()
                    .map(Line::from)
                    .collect(),
            };
            self.cache.update(width, || lines);
        }
        TextRows(self.cache.rows()).layout(width, top_row)
    }

    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize) {
        TextRows(self.cache.rows()).render(frame, area, top_row);
    }
}

impl PreviewContent<'_> {
    fn text(&self) -> Cow<'_, str> {
        match self.source.state {
            InspectionState::Loading => Cow::Borrowed("Loading configuration…"),
            InspectionState::Failed(error) => Cow::Borrowed(error),
            InspectionState::Ready(snapshot) => {
                if self.tab == PreviewTab::Diff {
                    let mut text = match self.source.draft_preview {
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
                    if let Some(error) = self.source.apply_error {
                        text.push_str("\n\nSave did not complete:\n");
                        text.push_str(error);
                    }
                    return Cow::Owned(text);
                }
                if self.tab == PreviewTab::Raw {
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
                let page_report = self.source.pages.inspection_report(snapshot);
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
