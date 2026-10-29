use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Clear, Paragraph},
    Frame,
};

use super::{ConfigurationPane, ConfigurationView, HitAreas, InspectionState};
use crate::application::configuration::ConfigurationTarget;
use crate::tui::features::configuration::page::PageRenderContext;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::theme;
use crate::tui::protocol::Component;

impl ConfigurationView {
    #[cfg(test)]
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_layer(frame, area, true);
    }

    pub(crate) fn render_layer(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        self.hits = HitAreas::default();
        self.preview.clear_mouse_area();
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
