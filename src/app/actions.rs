use std::collections::HashMap;
use std::time::{Duration, Instant};
use crate::nspawn::{StatusLevel, models::ContainerEntry};
use super::{App, DetailPane};

impl App {
    pub async fn refresh(&mut self) {
        if self.ui.show_wizard || self.ui.show_help || self.ui.show_power_menu { return; }
        self.dbus_active = self.manager.is_dbus_available().await;
        match self.manager.list_all().await {
            Ok(entries) => {
                let prev_name = self.entries.get(self.selected).map(|e| e.name.clone());
                self.entries = entries;
                self.selected = prev_name
                    .and_then(|name| self.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0)
                    .min(self.entries.len().saturating_sub(1));
            }
            Err(e) => log::error!("list_all: {}", e),
        }
        self.refresh_detail().await;

        // Check if any DBus call fell back to CLI during this refresh
        if self.dbus_active && self.manager.did_fallback() {
            self.set_status("⚡ DBus call failed — used CLI fallback".into(), StatusLevel::Warn);
        }
    }

    pub async fn refresh_detail(&mut self) {
        let entry: ContainerEntry = match self.entries.get(self.selected) {
            Some(e) => e.clone(),
            Option::None => {
                self.properties = Ok(HashMap::new());
                self.log_lines.clear();
                self.config_content = Option::None;
                return;
            }
        };
        match self.ui.detail_pane {
            DetailPane::Properties | DetailPane::Details => {
                match self.manager.get_properties(&entry.name).await {
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
                            if let Some(u) = &entry.usage { p.insert("Usage".into(), u.clone()); }
                            p.insert("State".into(), entry.state.label().into());
                        }
                        
                        self.properties = Ok(p);
                    }
                    Err(e) => { 
                        log::debug!("{e}"); 
                        self.properties = Err(e.to_string());
                    }
                }
            }
            DetailPane::Logs => {
                if entry.state.is_running() {
                    match self.manager.get_logs(&entry.name, 100).await {
                        Ok(l) => self.log_lines = l,
                        Err(e) => self.log_lines = vec![format!("Error: {e}")],
                    }
                } else {
                    self.log_lines = vec!["Container is not running.".into()];
                }
            }
            DetailPane::Config => {
                let new_content = crate::nspawn::config::NspawnConfig::load(&entry.name).map(|c| c.content);
                if self.config_content != new_content { self.ui.config_scroll = 0; }
                self.config_content = new_content;
            }
        }
    }

    pub fn set_status(&mut self, msg: String, level: StatusLevel) {
        self.status_message = Some((msg, level));
        self.status_expiry = Some(Instant::now() + Duration::from_secs(4));
    }

    pub fn select_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.entries.is_empty() {
            self.selected = if self.selected == 0 { self.entries.len() - 1 } else { self.selected - 1 };
        }
    }

    pub async fn action_start(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            if !e.state.is_running() {
                match self.manager.start(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Started {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    pub async fn action_poweroff(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            if e.state.is_running() {
                match self.manager.poweroff(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Powered off {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    pub async fn action_terminate(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            if e.state.is_running() {
                match self.manager.terminate(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Terminated {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    pub async fn action_reboot(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            if e.state.is_running() {
                match self.manager.reboot(&e.name).await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Rebooting {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    pub async fn action_kill(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            if e.state.is_running() {
                // For now, just send SIGTERM via kill
                match self.manager.kill(&e.name, "SIGTERM").await {
                    Ok(_) => {
                        let suffix = if self.manager.did_fallback() { " (via CLI fallback)" } else { "" };
                        self.set_status(format!("Sent SIGTERM to {}{}", e.name, suffix), StatusLevel::Success);
                    }
                    Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
                }
            }
        }
    }

    pub async fn action_enable(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            match self.manager.enable(&e.name).await {
                Ok(_) => self.set_status(format!("Enabled {}", e.name), StatusLevel::Success),
                Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
            }
        }
    }

    pub async fn action_disable(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            self.action_cooldown = Some(Instant::now());
            match self.manager.disable(&e.name).await {
                Ok(_) => self.set_status(format!("Disabled {}", e.name), StatusLevel::Success),
                Err(err) => self.set_status(format!("Error: {err}"), StatusLevel::Error),
            }
        }
    }

    pub fn selected_entry(&self) -> Option<&ContainerEntry> {
        self.entries.get(self.selected)
    }
}
