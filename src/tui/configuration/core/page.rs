//! Contract between the configuration workspace and independently owned pages.

use crossterm::event::KeyEvent;
use ratatui::{layout::Position, layout::Rect, Frame};

use super::super::pages::ConfigurationPageAction;
use super::{ConfigurationPane, InspectionState};
use crate::application::configuration::{
    ConfigurationDraft, ConfigurationDraftRequest, ConfigurationSnapshot, ConfigurationTarget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ConfigurationPageId {
    Wayland,
    X11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::tui::configuration) enum ConfigurationSectionId {
    HostIntegration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui::configuration) struct ConfigurationPageDescriptor {
    pub id: ConfigurationPageId,
    pub section: ConfigurationSectionId,
    pub label: &'static str,
}

pub(in crate::tui::configuration) struct PageRenderContext<'a> {
    pub state: &'a InspectionState,
    pub target: &'a ConfigurationTarget,
    pub pane: ConfigurationPane,
    pub draft: &'a ConfigurationDraft,
}

pub(in crate::tui::configuration) struct PageInteractionContext<'a> {
    pub target: &'a ConfigurationTarget,
    pub snapshot: Option<&'a ConfigurationSnapshot>,
}

#[derive(Default)]
pub(in crate::tui::configuration) struct PageInspectionReport {
    pub summary: Vec<String>,
    pub details: Vec<String>,
}

impl PageInspectionReport {
    pub fn append(&mut self, mut other: Self) {
        self.summary.append(&mut other.summary);
        if !other.details.is_empty() {
            if !self.details.is_empty() {
                self.details.push(String::new());
            }
            self.details.append(&mut other.details);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tui::configuration) enum PageInput {
    Key(KeyEvent),
    Click(Position),
    Scroll(bool),
}

pub(in crate::tui::configuration) enum PageRequest {
    None,
    Draft(ConfigurationDraftRequest),
    Action(ConfigurationPageAction),
    CleanDraftAction {
        action: ConfigurationPageAction,
        blocked_message: String,
    },
}

pub(in crate::tui::configuration) trait ConfigurationPageController {
    type Update;

    fn descriptor(&self) -> ConfigurationPageDescriptor;
    fn reset(&mut self);
    fn load(&mut self, snapshot: &ConfigurationSnapshot);
    fn inspection_report(&self, snapshot: &ConfigurationSnapshot) -> PageInspectionReport;
    fn render(&mut self, frame: &mut Frame, area: Rect, context: PageRenderContext<'_>);
    fn render_overlay(&mut self, frame: &mut Frame, area: Rect);
    fn modal_open(&self) -> bool;
    fn handle_input(
        &mut self,
        input: PageInput,
        context: PageInteractionContext<'_>,
    ) -> PageRequest;
    fn reject_action(&mut self, _message: String) {}
    fn update(&mut self, update: Self::Update);

    #[cfg(test)]
    fn test_state(&self) -> PageTestState;
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(in crate::tui::configuration) struct PageTestState {
    pub selected: Option<usize>,
    pub expanded: std::collections::BTreeSet<usize>,
    pub modal_open: bool,
    pub bindings: Vec<(Rect, usize)>,
    pub checkboxes: Vec<(Rect, usize)>,
    pub access: Rect,
}
