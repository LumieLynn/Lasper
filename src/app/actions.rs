use super::App;
use crate::nspawn::models::{ContainerEntry, ContainerState};
use crate::ui::views::detail_panel::DetailPane;
use std::time::{Duration, Instant};

impl App {
    pub async fn refresh(&mut self) {
        if self.ui.show_wizard || self.ui.show_help || self.ui.power_menu.is_some() {
            return;
        }
        self.data.dbus_active = self.data.manager.is_dbus_available().await;
        match self.data.manager.list_all().await {
            Ok(entries) => {
                let prev_name = self
                    .data
                    .entries
                    .get(self.data.selected)
                    .map(|e| e.name.clone());
                self.data.entries = self.merge_transitional_states(entries);
                self.data.properties_dirty = true;
                self.data.details_dirty = true;
                self.data.selected = prev_name
                    .and_then(|name| self.data.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0)
                    .min(self.data.entries.len().saturating_sub(1));
            }
            Err(e) => log::error!("list_all: {}", e),
        }
        self.refresh_detail().await;

        // Check if any DBus call fell back to CLI during this refresh
        if self.data.dbus_active {
            if let Some(reason) = self.data.manager.did_fallback() {
                self.set_status(
                    format!("⚡ DBus fallback: {}", reason),
                    crate::ui::StatusLevel::Warn,
                );
            }
        }
    }

    pub async fn refresh_detail(&mut self) {
        let entry: ContainerEntry = match self.data.entries.get(self.data.selected) {
            Some(e) => e.clone(),
            Option::None => {
                self.data.properties = Ok(crate::nspawn::models::MachineProperties::default());
                self.data.properties_dirty = true;
                self.data.log_manager.cleanup_all();
                self.data.config_content = Option::None;
                self.data.config_dirty = true;
                return;
            }
        };

        match self.ui.detail_panel.active_pane {
            DetailPane::Properties | DetailPane::Details => {
                match self.data.manager.get_properties(&entry.name, &entry).await {
                    Ok(p) => {
                        self.data.properties = Ok(p);
                        self.data.properties_dirty = true;
                        self.data.details_dirty = true;
                    }
                    Err(e) => {
                        log::debug!("{e}");
                        self.data.properties = Err(e.to_string());
                        self.data.properties_dirty = true;
                        self.data.details_dirty = true;
                    }
                }
            }
            DetailPane::Logs => {
                self.data.log_manager.get_or_create(&entry.name);

                if entry.state.is_running() {
                    if !self.data.log_manager.stream_is_active(&entry.name) {
                        if let Some((tx, fatal)) = self.data.log_manager.start_stream(&entry.name) {
                            let handle = self.data.manager.spawn_log_stream(&entry.name, tx, fatal);
                            self.data
                                .log_manager
                                .attach_stream_handle(&entry.name, handle);
                        }
                    }
                } else if self.data.log_manager.stop_stream(&entry.name) {
                    self.data
                        .log_manager
                        .push_line(&entry.name, "[CONTAINER STOPPED]");
                }
            }
            DetailPane::Config => {
                let new_content = match self.data.exec_ctx.nspawn.read(&entry.name).await {
                    Ok(config) => config.map(|config| config.content),
                    Err(error) => {
                        log::warn!(
                            "Failed to read .nspawn config for {}: {}",
                            entry.name,
                            error
                        );
                        None
                    }
                };
                if self.data.config_content != new_content {
                    self.ui.detail_panel.config_scroll = 0;
                    self.data.config_dirty = true;
                }
                self.data.config_content = new_content;
            }
            DetailPane::Metrics => {
                // Metrics are updated via AppEvent::MetricsUpdate
            }
        }
    }

    pub fn set_status(&mut self, msg: String, level: crate::ui::StatusLevel) {
        self.ui.status_message = Some((msg, level));
        self.ui.status_expiry = Some(Instant::now() + Duration::from_secs(4));
    }

    pub fn select_next(&mut self) {
        if !self.data.entries.is_empty() {
            self.data.selected = (self.data.selected + 1) % self.data.entries.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.data.entries.is_empty() {
            self.data.selected = if self.data.selected == 0 {
                self.data.entries.len() - 1
            } else {
                self.data.selected - 1
            };
        }
    }

    pub fn check_action_cooldown(&mut self) -> bool {
        if let Some(time) = self.data.action_cooldown {
            if Instant::now().duration_since(time) < Duration::from_secs(2) {
                return false;
            }
        }
        self.data.action_cooldown = Some(Instant::now());
        true
    }

    /// Generic helper for container actions to reduce boilerplate.
    fn perform_container_action<F, Fut>(
        &mut self,
        action_label: &'static str,
        transition: Option<ContainerState>,
        validate: impl FnOnce(&ContainerEntry) -> bool,
        action: F,
    ) where
        F: FnOnce(String, std::sync::Arc<dyn crate::nspawn::ops::NspawnManager>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = crate::nspawn::errors::Result<()>> + Send + 'static,
    {
        if !self.check_action_cooldown() {
            return;
        }

        let (name, manager, tx, previous_state) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };

            if !validate(e) {
                return;
            }

            let previous_state = transition.as_ref().map(|_| e.state.clone());
            if let Some(state) = transition {
                self.data
                    .transitions
                    .insert(e.name.clone(), (state.clone(), Instant::now()));
                // Apply immediately so the transition icon shows on the next
                // frame even when no watcher nudge arrives mid-operation.
                e.state = state;
            }

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (
                e.name.clone(),
                self.data.manager.clone(),
                tx,
                previous_state,
            )
        };

        let pm = self.permissions.clone();
        tokio::spawn(async move {
            let audit = match pm.request_elevation(action_label.to_string()).await {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ContainerActionFailed {
                            name,
                            previous_state,
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            };

            let res = audit.run(action(name.clone(), manager.clone())).await;
            // audit dropped — scope closed

            let suffix = match manager.did_fallback() {
                Some(reason) => format!(" (CLI fallback: {})", reason),
                None => String::new(),
            };

            match res {
                Ok(_) => {
                    let message = format!("{} {}{}", action_label, name, suffix);
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            message,
                            crate::ui::StatusLevel::Success,
                        ))
                        .await;
                }
                Err(error) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ContainerActionFailed {
                            name,
                            previous_state,
                            message: format!("Error: {error}"),
                        })
                        .await;
                }
            }
        });
    }

    pub(crate) fn rollback_container_transition(
        &mut self,
        name: &str,
        previous_state: Option<ContainerState>,
    ) {
        if previous_state.is_some() {
            self.data.transitions.remove(name);
        }
        if let Some(previous_state) = previous_state {
            if let Some(entry) = self
                .data
                .entries
                .iter_mut()
                .find(|entry| entry.name == name)
            {
                entry.state = previous_state;
            }
        }
    }

    pub fn action_start(&mut self) {
        self.perform_container_action(
            "Started",
            Some(ContainerState::Starting),
            |e| !e.state.is_running(),
            |name, manager| async move { manager.start(&name).await },
        );
    }

    pub fn action_poweroff(&mut self) {
        self.perform_container_action(
            "Powered off",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.poweroff(&name).await },
        );
    }

    pub fn action_terminate(&mut self) {
        self.perform_container_action(
            "Terminated",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.terminate(&name).await },
        );
    }

    pub fn action_reboot(&mut self) {
        self.perform_container_action(
            "Rebooting",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.reboot(&name).await },
        );
    }

    pub fn action_kill(&mut self) {
        self.perform_container_action(
            "Sent SIGKILL to",
            None,
            |e| e.state.is_running(),
            |name, manager| async move {
                manager
                    .kill(&name, crate::nspawn::models::AllowedSignal::Kill)
                    .await
            },
        );
    }

    pub fn action_remove(&mut self) {
        self.ui.delete_dialog = None;
        self.perform_container_action(
            "Removed",
            None,
            |e| !e.state.is_running(),
            |name, manager| async move { manager.remove(&name).await },
        );
    }

    pub fn action_enable(&mut self) {
        self.perform_container_action(
            "Enabled",
            None,
            |_| true,
            |name, manager| async move { manager.enable(&name).await },
        );
    }

    pub fn action_disable(&mut self) {
        self.perform_container_action(
            "Disabled",
            None,
            |_| true,
            |name, manager| async move { manager.disable(&name).await },
        );
    }

    pub async fn spawn_terminal(&mut self) {
        self.ui.prev_active_idx = self.ui.focus.active_idx;
        let rows = self.ui.pane_height.max(10);
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e.clone(),
            None => return,
        };

        match self
            .data
            .terminal
            .spawn(&entry, rows, &self.ui.app_tx, &self.data.exec_ctx)
            .await
        {
            Ok(_idx) => {
                self.ui.focus.active_idx = 2;
                self.set_status(
                    format!("Logged into {}", entry.name),
                    crate::ui::StatusLevel::Info,
                );
            }
            Err(msg) => {
                self.set_status(msg, crate::ui::StatusLevel::Error);
            }
        }
    }

    pub fn sync_terminal_to_selected(&mut self) {
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e,
            None => return,
        };
        self.data.terminal.sync_to_entry(&entry.name);
    }
}
