//! Registered configuration pages.
//!
//! This module is deliberately a registry, not an interaction coordinator.
//! Each controller owns its controls, hit testing, modal state, and runtime
//! result handling. The workspace talks to every page through `core::page`.

mod checklist;
mod request;
pub(in crate::tui::configuration) mod wayland;
pub(in crate::tui::configuration) mod x11;

pub(crate) use request::{
    ConfigurationPageAction, ConfigurationPageEvent, ConfigurationPageUpdate,
};

use ratatui::{layout::Rect, Frame};

use super::core::page::{
    ConfigurationPageController, ConfigurationPageDescriptor, ConfigurationPageId, PageInput,
    PageInspectionReport, PageInteractionContext, PageRenderContext, PageRequest,
};
use crate::application::configuration::ConfigurationSnapshot;
use wayland::WaylandPageState;
use x11::X11PageState;

pub(super) struct ConfigurationPages {
    controllers: Vec<RegisteredPage>,
    descriptors: Vec<ConfigurationPageDescriptor>,
}

enum RegisteredPage {
    Wayland(Box<WaylandPageState>),
    X11(Box<X11PageState>),
}

impl RegisteredPage {
    fn descriptor(&self) -> ConfigurationPageDescriptor {
        match self {
            Self::Wayland(page) => page.descriptor(),
            Self::X11(page) => page.descriptor(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Wayland(page) => page.reset(),
            Self::X11(page) => page.reset(),
        }
    }

    fn load(&mut self, snapshot: &ConfigurationSnapshot) {
        match self {
            Self::Wayland(page) => page.load(snapshot),
            Self::X11(page) => page.load(snapshot),
        }
    }

    fn inspection_report(&self, snapshot: &ConfigurationSnapshot) -> PageInspectionReport {
        match self {
            Self::Wayland(page) => page.inspection_report(snapshot),
            Self::X11(page) => page.inspection_report(snapshot),
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, context: PageRenderContext<'_>) {
        match self {
            Self::Wayland(page) => page.render(frame, area, context),
            Self::X11(page) => page.render(frame, area, context),
        }
    }

    fn render_overlay(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Wayland(page) => page.render_overlay(frame, area),
            Self::X11(page) => page.render_overlay(frame, area),
        }
    }

    fn modal_open(&self) -> bool {
        match self {
            Self::Wayland(page) => page.modal_open(),
            Self::X11(page) => page.modal_open(),
        }
    }

    fn handle_input(
        &mut self,
        input: PageInput,
        context: PageInteractionContext<'_>,
    ) -> PageRequest {
        match self {
            Self::Wayland(page) => page.handle_input(input, context),
            Self::X11(page) => page.handle_input(input, context),
        }
    }

    fn reject_action(&mut self, message: String) {
        match self {
            Self::Wayland(page) => page.reject_action(message),
            Self::X11(page) => page.reject_action(message),
        }
    }

    fn update(&mut self, update: ConfigurationPageUpdate) {
        match (self, update) {
            (Self::Wayland(page), ConfigurationPageUpdate::Wayland(update)) => page.update(update),
            (Self::X11(page), ConfigurationPageUpdate::X11(update)) => page.update(*update),
            _ => unreachable!("configuration update was routed to a different page"),
        }
    }

    #[cfg(test)]
    fn test_state(&self) -> super::core::page::PageTestState {
        match self {
            Self::Wayland(page) => page.test_state(),
            Self::X11(page) => page.test_state(),
        }
    }
}

impl Default for ConfigurationPages {
    fn default() -> Self {
        let controllers = vec![
            RegisteredPage::Wayland(Box::default()),
            RegisteredPage::X11(Box::default()),
        ];
        let descriptors = controllers
            .iter()
            .map(|controller| controller.descriptor())
            .collect();
        Self {
            controllers,
            descriptors,
        }
    }
}

impl ConfigurationPages {
    pub(super) fn descriptors(&self) -> &[ConfigurationPageDescriptor] {
        &self.descriptors
    }

    fn controller(&self, page: ConfigurationPageId) -> &RegisteredPage {
        self.controllers
            .iter()
            .find(|controller| controller.descriptor().id == page)
            .expect("navigation references a registered configuration page")
    }

    fn controller_mut(&mut self, page: ConfigurationPageId) -> &mut RegisteredPage {
        self.controllers
            .iter_mut()
            .find(|controller| controller.descriptor().id == page)
            .expect("navigation references a registered configuration page")
    }

    pub(super) fn reset(&mut self) {
        for controller in &mut self.controllers {
            controller.reset();
        }
    }

    pub(super) fn load(&mut self, snapshot: &ConfigurationSnapshot) {
        for controller in &mut self.controllers {
            controller.load(snapshot);
        }
    }

    pub(super) fn inspection_report(
        &self,
        snapshot: &ConfigurationSnapshot,
    ) -> PageInspectionReport {
        let mut report = PageInspectionReport::default();
        for controller in &self.controllers {
            report.append(controller.inspection_report(snapshot));
        }
        report
    }

    pub(super) fn render(
        &mut self,
        page: ConfigurationPageId,
        frame: &mut Frame,
        area: Rect,
        context: PageRenderContext<'_>,
    ) {
        self.controller_mut(page).render(frame, area, context);
    }

    pub(super) fn render_overlay(
        &mut self,
        page: ConfigurationPageId,
        frame: &mut Frame,
        area: Rect,
    ) {
        self.controller_mut(page).render_overlay(frame, area);
    }

    pub(super) fn modal_open(&self, page: ConfigurationPageId) -> bool {
        self.controller(page).modal_open()
    }

    pub(super) fn handle_input(
        &mut self,
        page: ConfigurationPageId,
        input: PageInput,
        context: PageInteractionContext<'_>,
    ) -> PageRequest {
        self.controller_mut(page).handle_input(input, context)
    }

    pub(super) fn reject_action(&mut self, page: ConfigurationPageId, message: String) {
        self.controller_mut(page).reject_action(message);
    }

    pub(super) fn update(&mut self, update: ConfigurationPageUpdate) {
        let page = update.page_id();
        self.controller_mut(page).update(update);
    }

    #[cfg(test)]
    pub(super) fn test_state(&self, page: ConfigurationPageId) -> super::core::page::PageTestState {
        self.controller(page).test_state()
    }
}
