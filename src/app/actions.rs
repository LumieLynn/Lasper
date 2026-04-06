use super::{App, DetailPane};
use crate::nspawn::{models::ContainerEntry, StatusLevel};
use std::collections::HashMap;
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
                self.data.selected = prev_name
                    .and_then(|name| self.data.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0)
                    .min(self.data.entries.len().saturating_sub(1));
            }
            Err(e) => log::error!("list_all: {}", e),
        }
        self.refresh_detail().await;

        // Check if any DBus call fell back to CLI during this refresh
        if self.data.dbus_active && self.data.manager.did_fallback() {
            self.set_status(
                "⚡ DBus call failed — used CLI fallback".into(),
                StatusLevel::Warn,
            );
        }
    }

    pub async fn refresh_detail(&mut self) {
        let entry: ContainerEntry = match self.data.entries.get(self.data.selected) {
            Some(e) => e.clone(),
            Option::None => {
                self.data.properties = Ok(HashMap::new());
                self.data.log_lines.clear();
                self.data.config_content = Option::None;
                return;
            }
        };
        match self.ui.detail_panel.active_pane {
            DetailPane::Properties | DetailPane::Details => {
                match self.data.manager.get_properties(&entry.name).await {
                    Ok(machine_props) => {
                        let mut p = machine_props.properties;
                        if !entry.all_addresses.is_empty() {
                            p.insert("IPAddresses".into(), entry.all_addresses.join(", "));
                        }
                        if let Some(ufs) = p.get("UnitFileState") {
                            p.insert("Enabled".into(), ufs.clone());
                        }
                        // Preserve storage type as "Type" and rename machinectl's "Type" to "Class"
                        if let Some(image_type) = &entry.image_type {
                            if let Some(machine_type) = p.remove("Type") {
                                p.insert("Class".into(), machine_type);
                            }
                            p.insert("Type".into(), image_type.clone());
                        }

                        // For stopped containers, manually ensure expected static fields
                        if !entry.state.is_running() {
                            p.insert("ReadOnly".into(), entry.readonly.to_string());
                            if let Some(u) = &entry.usage {
                                p.insert("Usage".into(), u.clone());
                            }
                            p.insert("State".into(), entry.state.label().into());
                        }

                        self.data.properties = Ok(p);
                    }
                    Err(e) => {
                        log::debug!("{e}");
                        self.data.properties = Err(e.to_string());
                    }
                }
            }
            DetailPane::Logs => {
                if entry.state.is_running() {
                    match self.data.manager.get_logs(&entry.name, 100).await {
                        Ok(l) => self.data.log_lines = l,
                        Err(e) => self.data.log_lines = vec![format!("Error: {e}")],
                    }
                } else {
                    self.data.log_lines = vec!["Container is not running.".into()];
                }
            }
            DetailPane::Config => {
                let new_content =
                    crate::nspawn::config::NspawnConfig::load(&entry.name).map(|c| c.content);
                if self.data.config_content != new_content {
                    self.ui.detail_panel.config_scroll = 0;
                }
                self.data.config_content = new_content;
            }
            DetailPane::Metrics => {
                // Metrics are updated via AppEvent::MetricsUpdate
            }
        }
    }

    pub fn set_status(&mut self, msg: String, level: StatusLevel) {
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

    pub fn action_start(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            if e.state.is_running() {
                return;
            }

            // State Latching: Optimistic UI update via transitions map
            self.data.transitions.insert(e.name.clone(), (crate::nspawn::models::ContainerState::Starting, Instant::now()));

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.start(&name).await {
                Ok(_) => {
                    let suffix = if manager.did_fallback() {
                        " (via CLI fallback)"
                    } else {
                        ""
                    };
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Started {}{}", name, suffix),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_poweroff(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            if !e.state.is_running() {
                return;
            }

            // State Latching: Optimistic UI update via transitions map
            self.data.transitions.insert(e.name.clone(), (crate::nspawn::models::ContainerState::Exiting, Instant::now()));

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.poweroff(&name).await {
                Ok(_) => {
                    let suffix = if manager.did_fallback() {
                        " (via CLI fallback)"
                    } else {
                        ""
                    };
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Powered off {}{}", name, suffix),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_terminate(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            if !e.state.is_running() {
                return;
            }

            // State Latching: Optimistic UI update via transitions map
            self.data.transitions.insert(e.name.clone(), (crate::nspawn::models::ContainerState::Exiting, Instant::now()));

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.terminate(&name).await {
                Ok(_) => {
                    let suffix = if manager.did_fallback() {
                        " (via CLI fallback)"
                    } else {
                        ""
                    };
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Terminated {}{}", name, suffix),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_reboot(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get_mut(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            if !e.state.is_running() {
                return;
            }

            // State Latching: Optimistic UI update: it will stop first
            self.data.transitions.insert(e.name.clone(), (crate::nspawn::models::ContainerState::Exiting, Instant::now()));

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.reboot(&name).await {
                Ok(_) => {
                    let suffix = if manager.did_fallback() {
                        " (via CLI fallback)"
                    } else {
                        ""
                    };
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Rebooting {}{}", name, suffix),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_kill(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            if !e.state.is_running() {
                return;
            }

            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.kill(&name, "SIGTERM").await {
                Ok(_) => {
                    let suffix = if manager.did_fallback() {
                        " (via CLI fallback)"
                    } else {
                        ""
                    };
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Sent SIGTERM to {}{}", name, suffix),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_enable(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.enable(&name).await {
                Ok(_) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Enabled {}", name),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }

    pub fn action_disable(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let (name, manager, tx) = {
            let e = match self.data.entries.get(self.data.selected) {
                Some(e) => e,
                None => return,
            };
            let tx = match &self.ui.app_tx {
                Some(tx) => tx.clone(),
                None => return,
            };
            (e.name.clone(), self.data.manager.clone(), tx)
        };

        tokio::spawn(async move {
            match manager.disable(&name).await {
                Ok(_) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Disabled {}", name),
                            StatusLevel::Success,
                        ))
                        .await;
                }
                Err(err) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Error: {err}"),
                            StatusLevel::Error,
                        ))
                        .await;
                }
            }
        });
    }
}
