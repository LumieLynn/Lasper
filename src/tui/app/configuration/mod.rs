//! Configuration workspace integration with the application event loop.

use super::App;
use crate::application::configuration::ConfigurationTarget;
use crate::application::inspection::ResourceInspectionError;
use crate::tui::configuration::{
    ConfigurationAction, ConfigurationPageEffect, ConfigurationPageEvent, ConfigurationView,
};

impl App {
    pub(super) fn handle_configuration_page_event(&mut self, event: ConfigurationPageEvent) {
        let event = if let Some(view) = &mut self.ui.configuration {
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
                if let Some(view) = self.ui.configuration.as_mut() {
                    view.update_page(update);
                }
            }
            ConfigurationPageEffect::OpenTerminal(request) => {
                let (page, machine, user, access) = request.into_launch();
                let opened = self.spawn_terminal_as_user(machine, user, access).await;
                if opened {
                    self.ui.configuration = None;
                } else {
                    let message = self
                        .ui
                        .status_message
                        .as_ref()
                        .map(|(message, _)| message.clone())
                        .unwrap_or_else(|| "Terminal could not be opened".into());
                    if let Some(view) = self.ui.configuration.as_mut() {
                        view.reject_page_action(page, message);
                    }
                }
            }
        }
    }

    pub(crate) fn open_configuration(&mut self, target: ConfigurationTarget) {
        self.ui.close_leader();
        self.ui.resource_action_menu = None;
        self.ui.configuration = Some(ConfigurationView::new(target));
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
            Err(error) => self.set_status(error.to_string(), crate::tui::StatusLevel::Warn),
        }
    }

    pub(crate) async fn handle_configuration_action(&mut self, action: ConfigurationAction) {
        match action {
            ConfigurationAction::Close => self.ui.configuration = None,
            ConfigurationAction::Help => self.ui.show_help = true,
            ConfigurationAction::Refresh => self.refresh_configuration(),
            ConfigurationAction::Preview { generation, edit } => {
                let Some(events) = self.ui.app_tx.clone() else {
                    if let Some(view) = self.ui.configuration.as_mut() {
                        view.finish_preview(
                            generation,
                            &edit.target,
                            Err(ResourceInspectionError::backend(
                                "Application event channel is unavailable",
                            )),
                        );
                    }
                    return;
                };
                let task = crate::tui::effects::configuration::preview(
                    self.data.configuration.clone(),
                    edit,
                    generation,
                    events,
                );
                if let Some(view) = self.ui.configuration.as_mut() {
                    view.track_preview(task);
                }
            }
            ConfigurationAction::Apply { generation, edit } => {
                let Some(events) = self.ui.app_tx.clone() else {
                    if let Some(view) = self.ui.configuration.as_mut() {
                        view.finish_apply(
                            generation,
                            &edit.target,
                            Err(ResourceInspectionError::backend(
                                "Application event channel is unavailable",
                            )),
                        );
                    }
                    return;
                };
                let task = crate::tui::effects::configuration::apply(
                    self.data.configuration.clone(),
                    edit,
                    generation,
                    events,
                );
                if let Some(view) = self.ui.configuration.as_mut() {
                    view.track_apply(task);
                }
            }
            ConfigurationAction::Page(action) => {
                let effect = self
                    .data
                    .configuration_executor
                    .start(action, self.ui.app_tx.clone());
                self.handle_configuration_page_effect(effect).await;
            }
            ConfigurationAction::Restart(machine) => {
                self.ui.configuration = None;
                self.action_runtime_named(
                    machine.as_str(),
                    crate::application::MachineRuntimeAction::Reboot,
                );
            }
            ConfigurationAction::None => {}
        }
    }

    pub(super) fn refresh_configuration(&mut self) {
        let Some(view) = self.ui.configuration.as_mut() else {
            return;
        };
        let query = self.ui.next_configuration_query;
        self.ui.next_configuration_query = query
            .checked_add(1)
            .expect("configuration query counter exhausted");
        view.begin_query(query);
        let target = view.target.clone();
        if let Some(events) = self.ui.app_tx.clone() {
            view.track_query(crate::tui::effects::configuration::inspect(
                self.data.configuration.clone(),
                target,
                query,
                events,
            ));
        } else {
            view.finish_query(
                query,
                &target,
                Err(ResourceInspectionError::backend(
                    "Application event channel is unavailable",
                )),
            );
        }
    }
}
