//! Configuration workspace integration with the application event loop.

use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationError, ConfigurationPreview, ConfigurationSnapshot,
    ConfigurationTarget,
};
use crate::tui::app::App;
use crate::tui::features::configuration::effects;
use crate::tui::features::configuration::{
    ConfigurationAction, ConfigurationPageEffect, ConfigurationPageEvent, ConfigurationView,
};

impl App {
    pub(in crate::tui::app) fn finish_configuration_query(
        &mut self,
        query: u64,
        target: ConfigurationTarget,
        result: Result<ConfigurationSnapshot, ConfigurationError>,
    ) {
        if let Some(view) = &mut self.ui.overlays.configuration {
            view.finish_query(query, &target, result);
        }
    }

    pub(in crate::tui::app) fn finish_configuration_preview(
        &mut self,
        generation: u64,
        target: ConfigurationTarget,
        result: Result<ConfigurationPreview, ConfigurationError>,
    ) {
        if let Some(view) = &mut self.ui.overlays.configuration {
            view.finish_preview(generation, &target, result);
        }
    }

    pub(in crate::tui::app) fn finish_configuration_apply(
        &mut self,
        generation: u64,
        target: ConfigurationTarget,
        result: Result<ConfigurationApplyReport, ConfigurationError>,
    ) {
        let saved = self
            .ui
            .overlays
            .configuration
            .as_mut()
            .and_then(|view| view.finish_apply(generation, &target, result));
        if let Some(message) = saved {
            self.set_status(
                message,
                crate::tui::foundation::status::StatusLevel::Success,
            );
            if !self
                .ui
                .overlays
                .configuration
                .as_ref()
                .is_some_and(ConfigurationView::restart_confirmation_pending)
            {
                self.refresh_configuration();
            }
        }
    }

    pub(in crate::tui::app) fn handle_configuration_page_event(
        &mut self,
        event: ConfigurationPageEvent,
    ) {
        let event = if let Some(view) = &mut self.ui.overlays.configuration {
            match view.finish_page_event(event) {
                Ok(()) => return,
                Err(event) => event,
            }
        } else {
            event
        };
        if let Some((message, level)) = event.detached_status() {
            self.set_status(message, level);
        }
    }

    async fn handle_configuration_page_effect(&mut self, effect: ConfigurationPageEffect) {
        match effect {
            ConfigurationPageEffect::Update(update) => {
                if let Some(view) = self.ui.overlays.configuration.as_mut() {
                    view.update_page(update);
                }
            }
            ConfigurationPageEffect::OpenTerminal(request) => {
                let (page, machine, user, access) = request.into_launch();
                let opened = self.spawn_terminal_as_user(machine, user, access).await;
                if opened {
                    self.ui.overlays.configuration = None;
                } else {
                    let message = self
                        .ui
                        .status_message
                        .as_ref()
                        .map(|(message, _)| message.clone())
                        .unwrap_or_else(|| "Terminal could not be opened".into());
                    if let Some(view) = self.ui.overlays.configuration.as_mut() {
                        view.reject_page_action(page, message);
                    }
                }
            }
        }
    }

    pub(crate) fn open_configuration(&mut self, target: ConfigurationTarget) {
        self.ui.close_leader();
        self.ui.overlays.resource_action_menu = None;
        self.ui.overlays.configuration = Some(ConfigurationView::new(target));
        self.refresh_configuration();
    }

    pub(crate) fn configure_focused_resource(&mut self) {
        let target = if let Some(image) = self.focused_image_resource() {
            ConfigurationTarget::for_image(image)
        } else if let Some(machine) = self.focused_machine_resource() {
            ConfigurationTarget::for_machine(machine)
        } else {
            return;
        };
        match target {
            Ok(target) => self.open_configuration(target),
            Err(error) => self.set_status(
                error.to_string(),
                crate::tui::foundation::status::StatusLevel::Warn,
            ),
        }
    }

    pub(crate) async fn handle_configuration_action(&mut self, action: ConfigurationAction) {
        match action {
            ConfigurationAction::Close => self.ui.overlays.configuration = None,
            ConfigurationAction::Help => self.ui.overlays.open_help(),
            ConfigurationAction::Refresh => self.refresh_configuration(),
            ConfigurationAction::Preview { generation, edit } => {
                let Some(events) = self.ui.app_tx.clone() else {
                    if let Some(view) = self.ui.overlays.configuration.as_mut() {
                        view.finish_preview(
                            generation,
                            &edit.target,
                            Err(ConfigurationError::failed(
                                "Application event channel is unavailable",
                            )),
                        );
                    }
                    return;
                };
                let task = effects::preview(
                    self.services.configuration.clone(),
                    edit,
                    generation,
                    events,
                );
                if let Some(view) = self.ui.overlays.configuration.as_mut() {
                    view.track_preview(task);
                }
            }
            ConfigurationAction::Apply { generation, edit } => {
                let Some(events) = self.ui.app_tx.clone() else {
                    if let Some(view) = self.ui.overlays.configuration.as_mut() {
                        view.finish_apply(
                            generation,
                            &edit.target,
                            Err(ConfigurationError::failed(
                                "Application event channel is unavailable",
                            )),
                        );
                    }
                    return;
                };
                let task = effects::apply(
                    self.services.configuration.clone(),
                    edit,
                    generation,
                    events,
                );
                if let Some(view) = self.ui.overlays.configuration.as_mut() {
                    view.track_apply(task);
                }
            }
            ConfigurationAction::Page(action) => {
                let effect = self
                    .services
                    .configuration_executor
                    .start(action, self.ui.app_tx.clone());
                self.handle_configuration_page_effect(effect).await;
            }
            ConfigurationAction::Restart(machine) => {
                self.ui.overlays.configuration = None;
                self.action_runtime_named(
                    machine.as_str(),
                    crate::application::MachineRuntimeAction::Reboot,
                );
            }
            ConfigurationAction::None => {}
        }
    }

    pub(in crate::tui::app) fn refresh_configuration(&mut self) {
        if self.ui.overlays.configuration.is_none() {
            return;
        }
        let query = self.ui.overlays.next_configuration_query();
        let view = self
            .ui
            .overlays
            .configuration
            .as_mut()
            .expect("checked above");
        view.begin_query(query);
        let target = view.target.clone();
        if let Some(events) = self.ui.app_tx.clone() {
            view.track_query(effects::inspect(
                self.services.configuration.clone(),
                target,
                query,
                events,
            ));
        } else {
            view.finish_query(
                query,
                &target,
                Err(ConfigurationError::failed(
                    "Application event channel is unavailable",
                )),
            );
        }
    }
}
