//! Compose wizard launch inputs and apply its cross-component UI effects.

use crate::tui::app::App;
use crate::tui::features::wizard::workflow::{
    WizardEffect, WizardEvent, WizardLaunch, WizardOpening, WizardServices,
};
use crate::tui::features::wizard::{StepAction, WizardInstanceId};
use crate::tui::foundation::status::StatusLevel;

impl App {
    fn wizard_services(&self) -> WizardServices {
        WizardServices {
            preparation: self.services.provisioning_preparation.clone(),
            provisioning: self.services.provisioning.clone(),
            host_operations: self.services.host_operations.clone(),
        }
    }

    pub(in crate::tui::app) fn begin_wizard(&mut self) {
        if !self.permissions.level().is_elevated() {
            self.set_status("Root required - run: lasper -e".into(), StatusLevel::Error);
            return;
        }
        if self.ui.overlays.wizard.is_some() || self.ui.overlays.wizard_opening.is_some() {
            return;
        }
        let Some(tx) = self.ui.app_tx.clone() else {
            self.set_status(
                "Internal error: application event channel is unavailable".into(),
                StatusLevel::Error,
            );
            return;
        };
        let id = WizardInstanceId::new(self.ui.overlays.next_wizard_instance);
        self.ui.overlays.next_wizard_instance =
            self.ui.overlays.next_wizard_instance.wrapping_add(1).max(1);
        let launch = WizardLaunch {
            id,
            machines: self.workspace.resources.catalog.entries.clone(),
            images: self.workspace.resources.catalog.images.clone(),
            permissions: self.permissions.level(),
            config: self.config.clone(),
        };
        self.ui.overlays.wizard_opening =
            Some(WizardOpening::start(launch, self.wizard_services(), tx));
    }

    pub(in crate::tui::app) async fn handle_wizard_event(&mut self, event: WizardEvent) {
        match event {
            WizardEvent::Opened { id, result } => {
                if self
                    .ui
                    .overlays
                    .wizard_opening
                    .as_ref()
                    .map(WizardOpening::id)
                    != Some(id)
                {
                    return;
                }
                let opening = self
                    .ui
                    .overlays
                    .wizard_opening
                    .take()
                    .expect("matched pending opening");
                match opening.finish(result, self.ui.app_tx.clone()) {
                    Ok(opened) => {
                        let mut session = opened.session;
                        session.draft.entries = self.workspace.resources.catalog.entries.clone();
                        session.draft.images = self.workspace.resources.catalog.images.clone();
                        self.ui.overlays.wizard = Some(session);
                        self.apply_wizard_effect(opened.effect).await;
                    }
                    Err(error) => self.set_status(
                        format!("Could not inspect provisioning capabilities: {error}"),
                        StatusLevel::Error,
                    ),
                }
            }
            WizardEvent::Completed { id, completion } => {
                let action = self
                    .ui
                    .overlays
                    .wizard
                    .as_mut()
                    .and_then(|session| session.complete(id, completion));
                if let Some(action) = action {
                    self.handle_wizard_action(action).await;
                }
            }
        }
    }

    pub(crate) async fn handle_wizard_action(&mut self, action: StepAction) {
        let services = self.wizard_services();
        let effect = match &mut self.ui.overlays.wizard {
            Some(session) => session.dispatch(action, &services, self.ui.app_tx.clone()),
            None if matches!(action, StepAction::Close) => WizardEffect::Close,
            None => return,
        };
        self.apply_wizard_effect(effect).await;
    }

    async fn apply_wizard_effect(&mut self, effect: WizardEffect) {
        match effect {
            WizardEffect::None => {}
            WizardEffect::Close => {
                self.close_wizard().await;
            }
            WizardEffect::Status(message, level) => self.set_status(message, level),
            WizardEffect::OpenDialog(dialog) => self.ui.overlays.active_dialog = Some(dialog),
            WizardEffect::CloseDialog => self.ui.overlays.active_dialog = None,
        }
    }

    pub(in crate::tui::app) async fn close_wizard(&mut self) {
        if let Some(opening) = self.ui.overlays.wizard_opening.take() {
            opening.shutdown().await;
        }
        self.ui.overlays.active_dialog = None;
        if let Some(mut session) = self.ui.overlays.wizard.take() {
            session.shutdown().await;
        }
    }
}
