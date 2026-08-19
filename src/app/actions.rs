use super::App;
use crate::nspawn::models::{ContainerEntry, ContainerState, ImageEntry, MachineProperties};
use crate::ui::views::detail_panel::{DetailPane, DetailTarget};
use std::time::{Duration, Instant};

impl App {
    pub async fn refresh(&mut self) {
        if self.ui.show_wizard || self.ui.show_help || self.ui.power_menu.is_some() {
            return;
        }
        self.data.dbus_active = self.data.manager.is_dbus_available().await;
        match self.data.manager.snapshot().await {
            Ok(snapshot) => {
                self.sync_snapshot(snapshot).await;
            }
            Err(error) => {
                log::error!("runtime snapshot: {}", error);
                self.set_status(
                    format!("Status refresh failed: {}", error),
                    crate::ui::StatusLevel::Warn,
                );
            }
        }
    }

    pub async fn refresh_detail(&mut self) {
        self.update_detail_target();
        let target = self.data.detail_target.clone();
        let Some(name) = target.name().map(str::to_string) else {
            if !matches!(target, DetailTarget::Empty) {
                return;
            }
            self.data.properties = Ok(crate::nspawn::models::MachineProperties::default());
            self.data.properties_dirty = true;
            self.data.log_manager.cleanup_all();
            self.data.config_content = Option::None;
            self.data.config_path = Option::None;
            self.data.config_dirty = true;
            self.data.unit_name = None;
            self.data.unit_drop_ins.clear();
            self.data.unit_dirty = true;
            return;
        };

        match target {
            DetailTarget::Machine(_) => {
                let entry = self
                    .data
                    .entries
                    .iter()
                    .find(|entry| entry.name == name)
                    .cloned()
                    .unwrap_or(ContainerEntry {
                        name: name.clone(),
                        state: ContainerState::Off,
                        address: None,
                        all_addresses: Vec::new(),
                    });
                match self.ui.detail_panel.active_pane {
                    DetailPane::Properties | DetailPane::Details => {
                        match self.data.manager.get_properties(&name, &entry).await {
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
                        self.data.log_manager.get_or_create(&name);
                        if entry.state == ContainerState::Running {
                            if !self.data.log_manager.stream_is_active(&name) {
                                if let Some((tx, fatal)) = self.data.log_manager.start_stream(&name)
                                {
                                    let handle =
                                        self.data.manager.spawn_log_stream(&name, tx, fatal);
                                    self.data.log_manager.attach_stream_handle(&name, handle);
                                }
                            }
                        } else if self.data.log_manager.stop_stream(&name) {
                            self.data
                                .log_manager
                                .push_line(&name, "[CONTAINER STOPPED]");
                        }
                    }
                    DetailPane::Config => {
                        self.read_nspawn_config(&name).await;
                    }
                    DetailPane::Metrics => {}
                    _ => {}
                }
            }
            DetailTarget::Image { .. } => match self.ui.detail_panel.active_pane {
                DetailPane::ImageOverview => {
                    self.data.properties = Ok(self.image_properties(&name));
                    self.data.properties_dirty = true;
                    self.data.details_dirty = true;
                }
                DetailPane::ImageConfig => self.read_nspawn_config(&name).await,
                DetailPane::ImageUnit => {
                    let has_corresponding_unit = match self
                        .data
                        .exec_ctx
                        .machine_inspection
                        .inspect_static(&name)
                        .await
                    {
                        Ok(Some(properties)) => {
                            self.data.properties = Ok(properties);
                            self.data.properties_dirty = true;
                            self.data.details_dirty = true;
                            true
                        }
                        Ok(None) => {
                            self.data.properties = Ok(MachineProperties::default());
                            self.data.properties_dirty = true;
                            self.data.details_dirty = true;
                            false
                        }
                        Err(error) => {
                            self.data.properties = Err(error.to_string());
                            self.data.properties_dirty = true;
                            self.data.details_dirty = true;
                            true
                        }
                    };
                    if has_corresponding_unit {
                        match self.data.exec_ctx.systemd_unit.read(&name).await {
                            Ok(unit) => {
                                self.data.unit_name = Some(unit.unit);
                                self.data.unit_drop_ins = unit.drop_ins;
                            }
                            Err(error) => {
                                log::debug!("Failed to read unit drop-ins for {name}: {error}");
                                self.data.unit_name =
                                    crate::nspawn::models::MachineName::new(&name)
                                        .ok()
                                        .map(|name| name.systemd_nspawn_unit());
                                self.data.unit_drop_ins.clear();
                            }
                        }
                    } else {
                        self.data.unit_name = None;
                        self.data.unit_drop_ins.clear();
                    }
                    self.data.unit_dirty = true;
                }
                _ => {}
            },
            DetailTarget::Empty => {}
        }
    }

    async fn read_nspawn_config(&mut self, name: &str) {
        let new_config = match self.data.exec_ctx.nspawn.inspect(name).await {
            Ok(config) => config,
            Err(error) => {
                log::warn!("Failed to read .nspawn config for {}: {}", name, error);
                None
            }
        };
        let new_path = new_config.as_ref().map(|config| config.path.clone());
        let new_content = new_config.map(|config| config.content);
        if self.data.config_content != new_content {
            self.ui.detail_panel.config_scroll = 0;
            self.data.config_dirty = true;
        }
        self.data.config_path = new_path;
        self.data.config_content = new_content;
    }

    fn image_properties(&self, name: &str) -> MachineProperties {
        let mut properties = MachineProperties::from_inspection(
            crate::nspawn::models::InspectionSource::RuntimeState,
            crate::nspawn::models::InspectionCompleteness::RuntimeOnly,
        );
        let image = self
            .data
            .images
            .iter()
            .chain(self.data.internal_images.iter())
            .find(|image| image.name == name);
        if let Some(image) = image {
            properties.insert("Image", "Name".into(), image.name.clone());
            properties.insert("Image", "Type".into(), image.image_type.clone());
            properties.insert(
                "Image",
                "ReadOnly".into(),
                if image.readonly { "yes" } else { "no" }.into(),
            );
            if let Some(usage) = &image.usage {
                properties.insert("Image", "Usage".into(), usage.clone());
            }
            if let Some(path) = &image.dbus_object_path {
                properties.insert("Image", "D-BusPath".into(), path.clone());
            }
        }
        properties
    }

    pub fn set_status(&mut self, msg: String, level: crate::ui::StatusLevel) {
        self.set_status_for(msg, level, Duration::from_secs(4));
    }

    pub fn set_status_for(
        &mut self,
        msg: String,
        level: crate::ui::StatusLevel,
        duration: Duration,
    ) {
        self.ui.status_message = Some((msg, level));
        self.ui.status_expiry = Some(Instant::now() + duration);
    }

    pub fn select_next(&mut self) {
        if self.image_is_focused() {
            if self.ui.image_list.shows_internal() {
                if !self.data.internal_images.is_empty() {
                    self.data.internal_image_selected =
                        (self.data.internal_image_selected + 1) % self.data.internal_images.len();
                }
            } else if !self.data.images.is_empty() {
                self.data.image_selected = (self.data.image_selected + 1) % self.data.images.len();
            }
        } else if !self.data.entries.is_empty() {
            self.data.selected = (self.data.selected + 1) % self.data.entries.len();
        }
    }

    pub fn select_prev(&mut self) {
        if self.image_is_focused() {
            if self.ui.image_list.shows_internal() && !self.data.internal_images.is_empty() {
                self.data.internal_image_selected = if self.data.internal_image_selected == 0 {
                    self.data.internal_images.len() - 1
                } else {
                    self.data.internal_image_selected - 1
                };
            } else if !self.ui.image_list.shows_internal() && !self.data.images.is_empty() {
                self.data.image_selected = if self.data.image_selected == 0 {
                    self.data.images.len() - 1
                } else {
                    self.data.image_selected - 1
                };
            }
        } else if !self.data.entries.is_empty() {
            self.data.selected = if self.data.selected == 0 {
                self.data.entries.len() - 1
            } else {
                self.data.selected - 1
            };
        }
    }

    pub(crate) fn image_is_focused(&self) -> bool {
        self.ui.focus.active_idx == 1
    }

    pub(crate) fn active_images(&self) -> (&[ImageEntry], usize) {
        if self.ui.image_list.shows_internal() {
            (
                &self.data.internal_images,
                self.data.internal_image_selected,
            )
        } else {
            (&self.data.images, self.data.image_selected)
        }
    }

    pub(crate) fn selected_image(&self) -> Option<&ImageEntry> {
        let (images, selected) = self.active_images();
        images.get(selected)
    }

    pub(crate) fn focused_image_resource(&self) -> Option<&ImageEntry> {
        if self.image_is_focused() {
            return self.selected_image();
        }
        if self.ui.focus.active_idx != 2 {
            return None;
        }
        let DetailTarget::Image { name, .. } = &self.data.detail_target else {
            return None;
        };
        self.data
            .images
            .iter()
            .chain(self.data.internal_images.iter())
            .find(|image| &image.name == name)
    }

    pub(crate) fn image_has_running_machine(&self, image: &ImageEntry) -> bool {
        !image.is_hidden()
            && self
                .data
                .entries
                .iter()
                .any(|entry| entry.name == image.name && entry.state == ContainerState::Running)
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
        let operation = self.data.exec_ctx.host_operations.begin();
        tokio::spawn(async move {
            let _operation = operation;
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
        let transition = self.data.transitions.remove(name);
        if let Some(previous_state) = previous_state {
            if let Some(entry) = self
                .data
                .entries
                .iter_mut()
                .find(|entry| entry.name == name)
            {
                entry.state = previous_state;
            }
        } else if matches!(transition, Some((ContainerState::Starting, _))) {
            let selected_name = self
                .data
                .entries
                .get(self.data.selected)
                .map(|entry| entry.name.clone());
            self.data
                .entries
                .retain(|entry| entry.name != name || entry.state != ContainerState::Starting);
            self.data.selected = selected_name
                .and_then(|selected| {
                    self.data
                        .entries
                        .iter()
                        .position(|entry| entry.name == selected)
                })
                .unwrap_or(0)
                .min(self.data.entries.len().saturating_sub(1));
        }
    }

    fn begin_image_start_transition(&mut self, name: &str) {
        let selected_name = self
            .data
            .entries
            .get(self.data.selected)
            .map(|entry| entry.name.clone());
        self.data
            .transitions
            .insert(name.to_string(), (ContainerState::Starting, Instant::now()));

        if let Some(entry) = self
            .data
            .entries
            .iter_mut()
            .find(|entry| entry.name == name)
        {
            entry.state = ContainerState::Starting;
        } else {
            self.data.entries.push(ContainerEntry {
                name: name.to_string(),
                state: ContainerState::Starting,
                address: None,
                all_addresses: Vec::new(),
            });
        }

        self.data.entries.sort();
        self.data.selected = selected_name
            .and_then(|selected| {
                self.data
                    .entries
                    .iter()
                    .position(|entry| entry.name == selected)
            })
            .or_else(|| {
                self.data
                    .entries
                    .iter()
                    .position(|entry| entry.name == name)
            })
            .unwrap_or(0);
    }

    pub fn action_start(&mut self) {
        if self.image_is_focused() {
            self.action_start_image();
            return;
        }
        let Some(name) = self
            .data
            .entries
            .get(self.data.selected)
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        let start_reservation = match self.data.image_lifecycle.reserve_machine_start(&name) {
            Ok(reservation) => reservation,
            Err(error) => {
                self.set_status(error.to_string(), crate::ui::StatusLevel::Warn);
                return;
            }
        };
        self.perform_container_action(
            "Started",
            Some(ContainerState::Starting),
            |e| !e.state.is_running(),
            move |name, manager| async move {
                let _start_reservation = start_reservation;
                manager.start(&name).await
            },
        );
    }

    pub fn action_poweroff(&mut self) {
        if self.image_is_focused() {
            return;
        }
        self.perform_container_action(
            "Powered off",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.poweroff(&name).await },
        );
    }

    pub fn action_terminate(&mut self) {
        if self.image_is_focused() {
            return;
        }
        self.perform_container_action(
            "Terminated",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.terminate(&name).await },
        );
    }

    pub fn action_reboot(&mut self) {
        if self.image_is_focused() {
            return;
        }
        self.perform_container_action(
            "Rebooting",
            Some(ContainerState::Exiting),
            |e| e.state.is_running(),
            |name, manager| async move { manager.reboot(&name).await },
        );
    }

    pub fn action_kill(&mut self) {
        if self.image_is_focused() {
            return;
        }
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
        if self.ui.delete_dialog.is_some() {
            self.action_remove_image();
            return;
        }
        self.set_status(
            "Focus Images to remove a machine image.".into(),
            crate::ui::StatusLevel::Info,
        );
    }

    fn action_start_image(&mut self) {
        if self.ui.image_list.shows_internal() {
            self.set_status(
                "Internal images cannot be started directly.".into(),
                crate::ui::StatusLevel::Warn,
            );
            return;
        }
        if !self.check_action_cooldown() {
            return;
        }
        let image = match self.data.images.get(self.data.image_selected) {
            Some(image) => image,
            None => return,
        };
        let name = image.name.clone();
        let is_mstack = image.image_type == "mstack";
        if let Some(state) = self
            .data
            .entries
            .iter()
            .find(|entry| entry.name == name && entry.state.is_running())
            .map(|entry| entry.state.label())
        {
            self.set_status(
                format!("{} is already {}.", name, state),
                crate::ui::StatusLevel::Info,
            );
            return;
        }
        let start_reservation = match self.data.image_lifecycle.reserve_machine_start(&name) {
            Ok(reservation) => reservation,
            Err(error) => {
                self.set_status(error.to_string(), crate::ui::StatusLevel::Warn);
                return;
            }
        };
        let manager = self.data.manager.clone();
        let tx = match &self.ui.app_tx {
            Some(tx) => tx.clone(),
            None => return,
        };
        self.begin_image_start_transition(&name);
        if is_mstack {
            self.set_status(
                format!(
                    "{} is an OCI application and may not be bootable; systemd will still try to start it.",
                    name
                ),
                crate::ui::StatusLevel::Warn,
            );
        }
        let pm = self.permissions.clone();
        let operation = self.data.exec_ctx.host_operations.begin();
        tokio::spawn(async move {
            let _start_reservation = start_reservation;
            let _operation = operation;
            let audit = match pm.request_elevation(format!("Start {}", name)).await {
                Ok(audit) => audit,
                Err(error) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ContainerActionFailed {
                            name,
                            previous_state: None,
                            message: format!("Start failed: {}", error),
                        })
                        .await;
                    return;
                }
            };
            let result = audit.run(manager.start(&name)).await;
            let event = match result {
                Ok(()) => crate::events::AppEvent::ActionDone(
                    format!("Started {}", name),
                    crate::ui::StatusLevel::Success,
                ),
                Err(error) => crate::events::AppEvent::ContainerActionFailed {
                    name,
                    previous_state: None,
                    message: format!("Start failed: {}", error),
                },
            };
            let _ = tx.send(event).await;
        });
    }

    fn action_remove_image(&mut self) {
        if !self.check_action_cooldown() {
            return;
        }
        let pending = match self.ui.delete_dialog.take() {
            Some(pending) => pending,
            None => return,
        };
        let name = pending.target().as_str().to_string();
        let cleanup_artifacts = pending.cleanup_artifacts();
        let image_still_exists = self
            .data
            .images
            .iter()
            .chain(self.data.internal_images.iter())
            .any(|image| image.name == name);
        if !image_still_exists {
            self.set_status(
                format!(
                    "Image {} changed before confirmation; refresh and try again.",
                    name
                ),
                crate::ui::StatusLevel::Warn,
            );
            return;
        }
        if self
            .data
            .entries
            .iter()
            .any(|machine| machine.name == name && machine.state.is_running())
        {
            self.set_status(
                format!("Stop machine '{}' before deleting its image.", name),
                crate::ui::StatusLevel::Warn,
            );
            return;
        }
        let image = match self
            .data
            .images
            .iter()
            .chain(self.data.internal_images.iter())
            .find(|image| image.name == name)
            .cloned()
        {
            Some(image) => image,
            None => {
                self.set_status(
                    format!("Image {} changed before removal started.", name),
                    crate::ui::StatusLevel::Warn,
                );
                return;
            }
        };
        let removal = match self
            .data
            .image_lifecycle
            .begin_remove(&image, cleanup_artifacts)
        {
            Ok(operation) => operation,
            Err(error) => {
                self.set_status(error.to_string(), crate::ui::StatusLevel::Warn);
                return;
            }
        };
        let tx = match &self.ui.app_tx {
            Some(tx) => tx.clone(),
            None => return,
        };
        let pm = self.permissions.clone();
        let operation = self.data.exec_ctx.host_operations.begin();
        tokio::spawn(async move {
            let _operation = operation;
            let audit = match pm.request_elevation(format!("Remove image {}", name)).await {
                Ok(audit) => audit,
                Err(error) => {
                    let _ = tx
                        .send(crate::events::AppEvent::ActionDone(
                            format!("Remove failed: {}", error),
                            crate::ui::StatusLevel::Error,
                        ))
                        .await;
                    return;
                }
            };
            let result = audit.run(async { Ok(removal.run().await) }).await;
            let event = match result {
                Err(error) => crate::events::AppEvent::ActionDone(
                    format!("Remove failed: {}", error),
                    crate::ui::StatusLevel::Error,
                ),
                Ok(crate::nspawn::ops::ImageRemovalOutcome::NotAttempted { reason, .. }) => {
                    crate::events::AppEvent::ActionDone(
                        format!("Remove was not attempted: {}", reason),
                        crate::ui::StatusLevel::Warn,
                    )
                }
                Ok(crate::nspawn::ops::ImageRemovalOutcome::Removed(report)) => {
                    let unit_warning = match &report.unit {
                        crate::nspawn::ops::image_lifecycle::UnitDisableReport::Failed(reason) => {
                            Some(reason.clone())
                        }
                        _ => None,
                    };
                    match report.artifacts {
                        crate::nspawn::ops::image_lifecycle::ArtifactCleanupReport::Removed => {
                            match unit_warning {
                                Some(reason) => crate::events::AppEvent::ActionDone(
                                    format!(
                                        "Removed image {} and Lasper artifacts; unit disable warning: {}",
                                        name, reason
                                    ),
                                    crate::ui::StatusLevel::Warn,
                                ),
                                None => crate::events::AppEvent::ActionDone(
                                    format!("Removed image {} and Lasper artifacts", name),
                                    crate::ui::StatusLevel::Success,
                                ),
                            }
                        }
                        crate::nspawn::ops::image_lifecycle::ArtifactCleanupReport::PreservedAmbiguous(
                            errors,
                        )
                        | crate::nspawn::ops::image_lifecycle::ArtifactCleanupReport::PartiallyRemoved(errors)
                        | crate::nspawn::ops::image_lifecycle::ArtifactCleanupReport::Failed(
                            errors,
                        ) => crate::events::AppEvent::ActionDone(
                            format!(
                                "Removed image {}; cleanup warning: {}{}",
                                name,
                                errors.join("; "),
                                unit_warning
                                    .as_deref()
                                    .map(|reason| format!("; unit disable: {reason}"))
                                    .unwrap_or_default()
                            ),
                            crate::ui::StatusLevel::Warn,
                        ),
                        _ => match unit_warning {
                            Some(reason) => crate::events::AppEvent::ActionDone(
                                format!("Removed image {}; unit disable warning: {}", name, reason),
                                crate::ui::StatusLevel::Warn,
                            ),
                            None => crate::events::AppEvent::ActionDone(
                                format!("Removed image {}", name),
                                crate::ui::StatusLevel::Success,
                            ),
                        },
                    }
                }
                Ok(crate::nspawn::ops::ImageRemovalOutcome::Rejected { reason, .. }) => {
                    crate::events::AppEvent::ActionDone(
                        format!("Remove rejected: {}", reason),
                        crate::ui::StatusLevel::Warn,
                    )
                }
                Ok(crate::nspawn::ops::ImageRemovalOutcome::Failed { reason, .. }) => {
                    crate::events::AppEvent::ActionDone(
                        format!("Remove failed: {}", reason),
                        crate::ui::StatusLevel::Error,
                    )
                }
                Ok(crate::nspawn::ops::ImageRemovalOutcome::OutcomeUnknown { reason, .. }) => {
                    crate::events::AppEvent::ActionDone(
                        format!("Removal outcome unknown: {}", reason),
                        crate::ui::StatusLevel::Warn,
                    )
                }
            };
            let _ = tx.send(event).await;
        });
    }

    pub fn action_enable(&mut self) {
        if self.image_is_focused() {
            return;
        }
        self.perform_container_action(
            "Enabled",
            None,
            |_| true,
            |name, manager| async move { manager.enable(&name).await },
        );
    }

    pub fn action_disable(&mut self) {
        if self.image_is_focused() {
            return;
        }
        self.perform_container_action(
            "Disabled",
            None,
            |_| true,
            |name, manager| async move { manager.disable(&name).await },
        );
    }

    pub async fn spawn_terminal(&mut self) {
        let entry = if self.focused_image_resource().is_some() {
            let Some(image) = self.focused_image_resource().cloned() else {
                self.set_status("No image selected.".into(), crate::ui::StatusLevel::Warn);
                return;
            };
            if image.is_hidden() {
                self.set_status(
                    "Internal images do not provide terminal sessions.".into(),
                    crate::ui::StatusLevel::Info,
                );
                return;
            }
            let Some(machine_idx) = self
                .data
                .entries
                .iter()
                .position(|entry| entry.name == image.name)
            else {
                if self
                    .data
                    .transitions
                    .get(&image.name)
                    .is_some_and(|(state, _)| *state == ContainerState::Starting)
                {
                    self.set_status(
                        format!("{} is still starting.", image.name),
                        crate::ui::StatusLevel::Info,
                    );
                } else {
                    self.set_status(
                        format!("{} is not running.", image.name),
                        crate::ui::StatusLevel::Info,
                    );
                }
                return;
            };
            let machine = self.data.entries[machine_idx].clone();
            if machine.state != ContainerState::Running {
                self.set_status(
                    format!(
                        "{} is {} and cannot accept a terminal.",
                        image.name,
                        machine.state.label()
                    ),
                    crate::ui::StatusLevel::Info,
                );
                return;
            }
            self.data.selected = machine_idx;
            machine
        } else {
            let Some(entry) = self.data.entries.get(self.data.selected).cloned() else {
                return;
            };
            entry
        };
        if self.ui.focus.active_idx != 3 {
            self.ui.prev_active_idx = self.ui.focus.active_idx.min(2);
        }
        let rows = self.ui.pane_height.max(10);

        match self
            .data
            .terminal
            .spawn(&entry, rows, &self.ui.app_tx, &self.data.exec_ctx)
            .await
        {
            Ok(session) => {
                self.set_focus_idx(3);
                self.refresh_detail().await;
                let message = match session.attach_kind {
                    crate::nspawn::sys::terminal_attach::TerminalAttachKind::Login => {
                        format!("Logged into {}", entry.name)
                    }
                    crate::nspawn::sys::terminal_attach::TerminalAttachKind::Namespace => {
                        format!("Attached to {} through its namespaces", entry.name)
                    }
                };
                self.set_status(message, crate::ui::StatusLevel::Info);
            }
            Err(msg) => {
                self.set_status(msg, crate::ui::StatusLevel::Error);
            }
        }
    }

    pub fn sync_terminal_to_selected(&mut self) {
        let was_focused = self.ui.focus.active_idx == 3;
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e,
            None => return,
        };
        self.data.terminal.sync_to_entry(&entry.name);
        if was_focused && !self.data.terminal.is_showing() {
            self.restore_non_terminal_focus();
        }
    }
}
