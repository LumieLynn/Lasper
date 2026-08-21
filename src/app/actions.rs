use super::App;
use crate::nspawn::models::{ContainerEntry, ContainerState, ImageEntry, MachineProperties};
use crate::ui::views::detail_panel::{DetailPane, DetailTarget};
use std::time::{Duration, Instant};

impl App {
    pub async fn refresh(&mut self) {
        if self.ui.show_wizard || self.ui.show_help || self.ui.power_menu.is_some() {
            return;
        }
        match self.data.runtime_catalog.snapshot().await {
            Ok(snapshot) => {
                self.sync_runtime_query(snapshot).await;
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
                        match self.data.runtime_catalog.inspect(&name, &entry).await {
                            Ok(query) => {
                                if let Some(fallback) = query.fallback {
                                    self.set_status(
                                        format!(
                                            "{} unavailable: {}; using {}",
                                            fallback.from.label(),
                                            fallback.reason,
                                            fallback.to.label()
                                        ),
                                        crate::ui::StatusLevel::Warn,
                                    );
                                }
                                self.data.properties = Ok(query.value);
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
                            if self.data.log_manager.can_start_stream(&name) {
                                match crate::domain::machine::MachineName::new(&name) {
                                    Ok(machine) => {
                                        match self.data.session_service.open_journal(machine).await
                                        {
                                            Ok(handle) => {
                                                self.data.log_manager.attach_stream(&name, handle)
                                            }
                                            Err(error) => {
                                                self.data.log_manager.push_line(
                                                    &name,
                                                    format!("Log stream error: {error}"),
                                                );
                                                if let Some(hint) = error.hint() {
                                                    self.data.log_manager.push_line(&name, hint);
                                                }
                                                self.data.log_manager.mark_stream_failed(&name);
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        self.data
                                            .log_manager
                                            .push_line(&name, format!("Log stream error: {error}"));
                                        self.data.log_manager.mark_stream_failed(&name);
                                    }
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

    fn perform_machine_action(
        &mut self,
        name: String,
        action: crate::nspawn::ops::MachineAction,
        observed_state: Option<ContainerState>,
    ) -> bool {
        if !self.check_action_cooldown() {
            return false;
        }
        let tx = match &self.ui.app_tx {
            Some(tx) => tx.clone(),
            None => return false,
        };
        let operation = match self
            .data
            .machine_lifecycle
            .begin(&name, action, observed_state)
        {
            Ok(operation) => operation,
            Err(rejection) => {
                self.set_status(
                    format!("{}: {}", name, rejection),
                    crate::ui::StatusLevel::Warn,
                );
                return false;
            }
        };
        self.apply_machine_projection();

        let pm = self.permissions.clone();
        let host_operation = self.data.exec_ctx.host_operations.begin();
        tokio::spawn(async move {
            let _host_operation = host_operation;
            let audit = match pm
                .request_elevation(format!("{} {}", action.audit_label(), name))
                .await
            {
                Ok(a) => a,
                Err(error) => {
                    drop(operation);
                    let _ = tx
                        .send(crate::events::AppEvent::MachineActionFinished(
                            crate::nspawn::ops::MachineLifecycleOutcome {
                                machine: crate::nspawn::models::MachineName::new(name)
                                    .expect("lifecycle operation already validated the name"),
                                action,
                                result: crate::nspawn::ops::MachineLifecycleResult::NotAttempted(
                                    error.to_string(),
                                ),
                                route: None,
                                fallback: None,
                            },
                        ))
                        .await;
                    return;
                }
            };
            let outcome = audit
                .run(async move { Ok(operation.run().await) })
                .await
                .expect("machine lifecycle operation returns a semantic outcome");
            let _ = tx
                .send(crate::events::AppEvent::MachineActionFinished(outcome))
                .await;
        });
        true
    }

    fn apply_machine_projection(&mut self) {
        let selected_name = self
            .data
            .entries
            .get(self.data.selected)
            .map(|entry| entry.name.clone());
        self.data.entries = self
            .data
            .machine_lifecycle
            .project_machines(std::mem::take(&mut self.data.entries));
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

    pub fn action_start(&mut self) {
        if self.image_is_focused() {
            self.action_start_image();
            return;
        }
        let Some(entry) = self.data.entries.get(self.data.selected).cloned() else {
            return;
        };
        self.perform_machine_action(
            entry.name,
            crate::nspawn::ops::MachineAction::Start,
            Some(entry.state),
        );
    }

    pub fn action_poweroff(&mut self) {
        if self.image_is_focused() {
            return;
        }
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Poweroff,
                Some(entry.state),
            );
        }
    }

    pub fn action_terminate(&mut self) {
        if self.image_is_focused() {
            return;
        }
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Terminate,
                Some(entry.state),
            );
        }
    }

    pub fn action_reboot(&mut self) {
        if self.image_is_focused() {
            return;
        }
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Reboot,
                Some(entry.state),
            );
        }
    }

    pub fn action_kill(&mut self) {
        if self.image_is_focused() {
            return;
        }
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Kill {
                    signal: crate::nspawn::models::AllowedSignal::Kill,
                },
                Some(entry.state),
            );
        }
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
        if self.perform_machine_action(name.clone(), crate::nspawn::ops::MachineAction::Start, None)
            && is_mstack
        {
            self.set_status(
                format!(
                    "{} is an OCI application and may not be bootable; systemd will still try to start it.",
                    name
                ),
                crate::ui::StatusLevel::Warn,
            );
        }
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
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Enable,
                Some(entry.state),
            );
        }
    }

    pub fn action_disable(&mut self) {
        if self.image_is_focused() {
            return;
        }
        if let Some(entry) = self.data.entries.get(self.data.selected).cloned() {
            self.perform_machine_action(
                entry.name,
                crate::nspawn::ops::MachineAction::Disable,
                Some(entry.state),
            );
        }
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
                self.set_status(
                    format!("{} is not running.", image.name),
                    crate::ui::StatusLevel::Info,
                );
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
            .spawn(&entry, rows, &self.ui.app_tx)
            .await
        {
            Ok(session) => {
                self.set_focus_idx(3);
                self.refresh_detail().await;
                let message = match session.attach_kind {
                    crate::domain::session::TerminalAttachmentKind::Login => {
                        format!("Logged into {}", entry.name)
                    }
                    crate::domain::session::TerminalAttachmentKind::Namespace => {
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
