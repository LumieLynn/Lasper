//! Bind the inspector workflow to the active workspace target and pane.

use crate::tui::app::App;
use crate::tui::features::workspace::inspector::refresh::{
    DetailRefreshCompletion, DetailRefreshServices,
};
use crate::tui::features::workspace::inspector::state::DetailRefreshEffects;

impl App {
    /// Queue the visible target; input handlers never perform host reads.
    pub(crate) fn request_detail_refresh(&mut self) {
        let machine = self
            .terminal
            .active_session()
            .map(|session| session.machine_name.as_str());
        self.workspace.request_inspection(machine);
    }

    pub(crate) fn start_detail_refresh(
        &mut self,
        completion_tx: &tokio::sync::mpsc::Sender<DetailRefreshCompletion>,
    ) {
        let services = self.detail_refresh_services();
        let effects = self.workspace.start_inspection(services, completion_tx);
        self.apply_detail_effects(effects);
    }

    pub(crate) fn apply_detail_refresh(&mut self, completion: DetailRefreshCompletion) {
        let effects = self.workspace.inspector.apply_refresh(completion);
        self.apply_detail_effects(effects);
    }

    fn apply_detail_effects(&mut self, effects: DetailRefreshEffects) {
        if let Some((message, level)) = effects.status {
            self.set_status(message, level);
        }
    }

    fn detail_refresh_services(&self) -> DetailRefreshServices {
        DetailRefreshServices {
            runtime_catalog: self.services.runtime_catalog.clone(),
            session_service: self.services.session_service.clone(),
            resource_inspection: self.services.resource_inspection.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn refresh_detail_now(&mut self) {
        let services = self.detail_refresh_services();
        let machine = self
            .terminal
            .active_session()
            .map(|session| session.machine_name.as_str());
        let effects = self.workspace.inspect_now(machine, services).await;
        self.apply_detail_effects(effects);
    }
}
