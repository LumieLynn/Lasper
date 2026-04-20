use super::{App, DetailPane};
use crate::nspawn::{models::ContainerEntry, models::ContainerState};
use ratatui::text::Line;
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
                self.data.log_lines.clear();
                self.data.logs_dirty = true;
                self.data.config_content = Option::None;
                self.data.config_dirty = true;
                if let Some((_, handle)) = self.data.log_stream.take() {
                    handle.abort();
                }
                return;
            }
        };

        // Stop log stream if we are not in the Logs pane
        if self.ui.detail_panel.active_pane != DetailPane::Logs {
            if let Some((_, handle)) = self.data.log_stream.take() {
                handle.abort();
            }
        }

        match self.ui.detail_panel.active_pane {
            DetailPane::Properties | DetailPane::Details => {
                match self.data.manager.get_properties(&entry.name).await {
                    Ok(mut p) => {
                        if !entry.all_addresses.is_empty() {
                            p.insert("Machine", "IPAddresses".into(), entry.all_addresses.join(", "));
                        }
                        if let Some(ufs) = p.get_group_mut("Systemd Unit").get("UnitFileState") {
                            let ufs = ufs.clone();
                            p.insert("Systemd Unit", "Enabled".into(), ufs);
                        }
                        // Preserve storage type as "Type" and rename machinectl's "Type" to "Class"
                        if let Some(image_type) = &entry.image_type {
                            if let Some(machine_type) = p.get_group_mut("Machine").remove("Type") {
                                p.insert("Machine", "Class".into(), machine_type);
                            }
                            p.insert("Machine", "Type".into(), image_type.clone());
                        }

                        // For stopped containers, manually ensure expected static fields
                        if !entry.state.is_running() {
                            p.insert("Machine", "ReadOnly".into(), entry.readonly.to_string());
                            if let Some(u) = &entry.usage {
                                p.insert("Machine", "Usage".into(), u.clone());
                            }
                            p.insert("Machine", "State".into(), entry.state.label().into());
                        }

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
                if entry.state.is_running() {
                    let needs_new_stream = match &self.data.log_stream {
                        Some((name, _)) if name == &entry.name => false,
                        _ => true,
                    };

                    if needs_new_stream {
                        // Stop old stream
                        if let Some((_, handle)) = self.data.log_stream.take() {
                            handle.abort();
                        }
                        self.data.log_lines.clear();
                        self.data.logs_dirty = true;
                        if let Some(tx) = &self.ui.app_tx {
                            let handle = self.data.manager.spawn_log_stream(&entry.name, tx.clone());
                            self.data.log_stream = Some((entry.name.clone(), handle));
                        }
                    }
                } else {
                    if let Some((_, handle)) = self.data.log_stream.take() {
                        handle.abort();
                    }
                    self.data.log_lines.clear();
                    self.data.log_lines.push_back(Line::from("Container is not running."));
                    self.data.logs_dirty = true;
                }
            }
            DetailPane::Config => {
                let new_content =
                    crate::nspawn::adapters::config::nspawn_file::NspawnConfig::load(&entry.name).await.map(|c| c.content);
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

        let (name, manager, tx) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };

            if !validate(e) {
                return;
            }

            if let Some(state) = transition {
                self.data.transitions
                    .insert(e.name.clone(), (state, Instant::now()));
            }

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            let res = action(name.clone(), manager.clone()).await;
            let suffix = match manager.did_fallback() {
                Some(reason) => format!(" (CLI fallback: {})", reason),
                None => String::new(),
            };

            let (msg, level) = match res {
                Ok(_) => (
                    format!("{} {}{}", action_label, name, suffix),
                    crate::ui::StatusLevel::Success,
                ),
                Err(err) => (format!("Error: {err}"), crate::ui::StatusLevel::Error),
            };

            let _ = tx
                .send(crate::events::AppEvent::ActionDone(msg, level))
                .await;
        });
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
            "Sent SIGTERM to",
            None,
            |e| e.state.is_running(),
            |name, manager| async move { manager.kill(&name, "SIGTERM").await },
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

    pub fn spawn_terminal(&mut self) {
        // Use current UI height for initial rows, default columns
        let rows = self.ui.pane_height.max(10);
        let cols = 80; // Standard fallback

        // Get the selected container
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e,
            None => return,
        };

        // Only for running containers.
        if !entry.state.is_running() {
            self.set_status(
                format!("Container {} is not running", entry.name),
                crate::ui::StatusLevel::Error,
            );
            return;
        }

        // Check if session already exists
        if let Some(idx) = self.data.terminal_sessions.iter().position(|s| s.container_name == entry.name) {
            self.data.active_terminal_idx = idx;
            self.ui.show_terminal = true;
            self.ui.active_panel = crate::app::ActivePanel::TerminalPanel;
            return;
        }

        let cmd = "machinectl";
        let args = vec!["login".to_string(), entry.name.clone()];
        let container_name = entry.name.clone();

        if let Some(tx) = &self.ui.app_tx {
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            match crate::nspawn::adapters::comm::pty::spawn_terminal(
                cmd,
                &arg_refs,
                cols,
                rows,
                tx.clone(),
            ) {
                Ok((term, pty_tx, handle)) => {
                    let session = super::TerminalSession {
                        container_name,
                        terminal: term,
                        pty_tx,
                        handle,
                        scroll_offset: 0,
                        insert_mode: true, // Default to insert mode on spawn
                    };
                    self.data.terminal_sessions.push(session);
                    self.data.active_terminal_idx = self.data.terminal_sessions.len() - 1;
                    self.ui.show_terminal = true;
                    self.ui.active_panel = crate::app::ActivePanel::TerminalPanel;
                    self.set_status(format!("Logged into {}", args[1]), crate::ui::StatusLevel::Info);
                }
                Err(e) => {
                    self.set_status(
                        format!("Failed to spawn terminal: {}", e),
                        crate::ui::StatusLevel::Error,
                    );
                }
            }
        }
    }

    pub fn close_active_terminal(&mut self) {
        if self.data.terminal_sessions.is_empty() {
            self.ui.show_terminal = false;
            return;
        }

        let mut session = self.data.terminal_sessions.remove(self.data.active_terminal_idx);
        session.handle.abort();
        
        if self.data.active_terminal_idx >= self.data.terminal_sessions.len() && !self.data.terminal_sessions.is_empty() {
            self.data.active_terminal_idx = self.data.terminal_sessions.len() - 1;
        }

        if self.data.terminal_sessions.is_empty() {
            self.ui.show_terminal = false;
            self.ui.active_panel = crate::app::ActivePanel::DetailPanel;
        }
    }

    pub fn sync_terminal_to_selected(&mut self) {
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e,
            None => return,
        };

        if let Some(idx) = self.data.terminal_sessions.iter().position(|s| s.container_name == entry.name) {
            self.data.active_terminal_idx = idx;
        } else {
            // Hide terminal if no session for the selected container
            self.ui.show_terminal = false;
        }
    }

    pub fn cleanup_all_terminals(&mut self) {
        for mut session in self.data.terminal_sessions.drain(..) {
            session.handle.abort();
        }
    }
}
