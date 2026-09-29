//! Page-independent configuration workspace contracts.
//! Concrete actions, completion messages, and access policy belong to each page.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::{layout::Position, Frame};

use crate::application::configuration::{
    ConfigurationDraft, ConfigurationDraftRequest, ConfigurationSnapshot, ConfigurationTarget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationPane {
    Navigation,
    Content,
    Preview,
}

pub(crate) enum InspectionState {
    Loading,
    Ready(Box<ConfigurationSnapshot>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ConfigurationPageId(&'static str);

impl ConfigurationPageId {
    pub(in crate::tui::configuration) const fn new(id: &'static str) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::tui::configuration) struct ConfigurationSection {
    pub key: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui::configuration) struct ConfigurationPageDescriptor {
    pub id: ConfigurationPageId,
    pub section: ConfigurationSection,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui::configuration) enum PageInput {
    Key(KeyEvent),
    Click(Position),
    Scroll(bool),
}

pub(in crate::tui::configuration) enum PageRequest<A> {
    None,
    Draft(ConfigurationDraftRequest),
    Action(A),
    CleanDraftAction { action: A, blocked_message: String },
}

pub(in crate::tui::configuration) trait ConfigurationPageController {
    type Action;
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
    ) -> PageRequest<Self::Action>;
    fn reject_action(&mut self, _message: String) {}
    fn update(&mut self, update: Self::Update);
}

impl<A> PageRequest<A> {
    pub fn map_action<B>(self, map: impl FnOnce(A) -> B) -> PageRequest<B> {
        match self {
            Self::None => PageRequest::None,
            Self::Draft(request) => PageRequest::Draft(request),
            Self::Action(action) => PageRequest::Action(map(action)),
            Self::CleanDraftAction {
                action,
                blocked_message,
            } => PageRequest::CleanDraftAction {
                action: map(action),
                blocked_message,
            },
        }
    }
}
