use super::detail_refresh::{
    DetailRefreshCompletion, DetailRefreshJob, DetailRefreshResult, DetailRefreshServices,
    DetailRefreshWork,
};
use super::App;
use crate::application::machine_lifecycle::MachineOperation;
use crate::application::{MachineLifecycleAction, MachineRuntimeAction, NspawnUnitAction};
use crate::domain::inspection::{InspectionCompleteness, InspectionSource, MachineProperties};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineState};
use crate::tui::views::detail_panel::{DetailPane, DetailTarget};
use std::time::{Duration, Instant};

enum PreparedDetailRefresh {
    Ready(DetailRefreshCompletion),
    Job(DetailRefreshJob),
}

impl App {
    pub(crate) async fn show_shell_dialog(&mut self) {
        let entry = if let Some(image) = self.focused_image_resource() {
            self.data
                .entries
                .iter()
                .find(|entry| entry.name == image.name)
                .cloned()
        } else {
            self.data.entries.get(self.data.selected).cloned()
        };
        let Some(entry) = entry else {
            self.set_status(
                "Select a running machine before opening a shell.".into(),
                crate::tui::StatusLevel::Info,
            );
            return;
        };
        if entry.state != MachineState::Running {
            self.set_status(
                format!("{} is not running.", entry.name),
                crate::tui::StatusLevel::Info,
            );
            return;
        }
        let Ok(machine) = MachineName::new(&entry.name) else {
            self.set_status(
                format!("{} is not a valid nspawn machine name.", entry.name),
                crate::tui::StatusLevel::Error,
            );
            return;
        };
        let wayland_sockets = self
            .data
            .session_service
            .discover_host_wayland_sockets()
            .await;
        self.ui.active_dialog = Some(Box::new(
            crate::tui::widgets::dialogs::shell::ShellDialog::new(
                machine,
                String::new(),
                wayland_sockets,
            ),
        ));
    }

    pub(crate) async fn spawn_terminal_as_user(
        &mut self,
        machine: MachineName,
        user: crate::application::sessions::ValidatedGuestUserName,
        wayland: crate::application::sessions::WaylandShellRequest,
    ) -> bool {
        let Some(entry) = self
            .data
            .entries
            .iter()
            .find(|entry| entry.name == machine.as_str())
            .cloned()
        else {
            self.set_status(
                format!("{} is no longer running.", machine),
                crate::tui::StatusLevel::Warn,
            );
            return false;
        };
        let rows = self.ui.pane_height.max(10);
        match self
            .data
            .terminal
            .spawn_as_user(&entry, user.clone(), wayland, rows, &self.ui.app_tx)
            .await
        {
            Ok(_) => {
                if !self.ui.focus.is_terminal() {
                    self.ui.prev_focus = self.ui.focus;
                }
                self.set_focus(crate::tui::app::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                self.set_status(
                    format!("Opened {}@{}", user, machine),
                    crate::tui::StatusLevel::Info,
                );
                true
            }
            Err(message) => {
                self.set_status(message, crate::tui::StatusLevel::Error);
                false
            }
        }
    }

    /// Request a runtime refresh without waiting on host I/O.
    ///
    /// The runtime observer owns the snapshot query and publishes the result
    /// back to the main loop. Keeping this call synchronous prevents a slow
    /// D-Bus or CLI backend from blocking keyboard and mouse dispatch.
    pub fn refresh(&mut self) {
        if self.ui.show_wizard || self.ui.show_help || self.ui.resource_action_menu.is_some() {
            return;
        }
        self.data.runtime_catalog.invalidate();
    }

    /// Replace any queued detail read with a snapshot of the currently visible
    /// target. No host I/O occurs on the input-handler call stack.
    pub(crate) fn request_detail_refresh(&mut self) {
        self.update_detail_target();
        self.data.detail_refresh.request(
            self.data.detail_target.clone(),
            self.ui.detail_panel.active_pane(),
        );
    }

    /// Start at most one queued detail read. Repeated input before this point
    /// has already coalesced to the latest ticket.
    pub(crate) fn start_detail_refresh(
        &mut self,
        completion_tx: &tokio::sync::mpsc::Sender<DetailRefreshCompletion>,
    ) {
        let Some(prepared) = self.prepare_detail_refresh() else {
            return;
        };
        match prepared {
            PreparedDetailRefresh::Ready(completion) => {
                self.apply_detail_refresh(completion);
            }
            PreparedDetailRefresh::Job(job) => {
                let completion_tx = completion_tx.clone();
                let services = self.detail_refresh_services();
                tokio::spawn(async move {
                    let completion = job.execute(services).await;
                    let _ = completion_tx.send(completion).await;
                });
            }
        }
    }

    fn prepare_detail_refresh(&mut self) -> Option<PreparedDetailRefresh> {
        let ticket = self.data.detail_refresh.take_pending()?;
        if ticket.target != self.data.detail_target
            || ticket.pane != self.ui.detail_panel.active_pane()
        {
            return Some(PreparedDetailRefresh::Ready(DetailRefreshCompletion {
                ticket,
                result: DetailRefreshResult::Noop,
            }));
        }

        let ready = |ticket, result| {
            PreparedDetailRefresh::Ready(DetailRefreshCompletion { ticket, result })
        };
        let job = |ticket, work| PreparedDetailRefresh::Job(DetailRefreshJob { ticket, work });

        let prepared = match &ticket.target {
            DetailTarget::Empty => ready(ticket.clone(), DetailRefreshResult::Empty),
            DetailTarget::Machine(name) => {
                let Some(entry) = self
                    .data
                    .entries
                    .iter()
                    .find(|entry| entry.name == *name)
                    .cloned()
                else {
                    return Some(ready(ticket, DetailRefreshResult::Empty));
                };
                match ticket.pane {
                    DetailPane::Properties | DetailPane::Details => job(
                        ticket.clone(),
                        DetailRefreshWork::MachineProperties {
                            name: name.clone(),
                            entry,
                        },
                    ),
                    DetailPane::Logs => {
                        self.data.log_manager.get_or_create(name);
                        if entry.state == MachineState::Running
                            && self.data.log_manager.can_start_stream(name)
                        {
                            job(
                                ticket.clone(),
                                DetailRefreshWork::Journal { name: name.clone() },
                            )
                        } else {
                            if entry.state != MachineState::Running
                                && self.data.log_manager.stop_stream(name)
                            {
                                self.data.log_manager.push_line(name, "[CONTAINER STOPPED]");
                            }
                            ready(ticket.clone(), DetailRefreshResult::Noop)
                        }
                    }
                    DetailPane::Config => {
                        job(ticket.clone(), DetailRefreshWork::MachineConfig { entry })
                    }
                    DetailPane::Metrics
                    | DetailPane::ImageOverview
                    | DetailPane::ImageConfig
                    | DetailPane::ImageUnit => ready(ticket.clone(), DetailRefreshResult::Noop),
                }
            }
            DetailTarget::Image { name, .. } => match ticket.pane {
                DetailPane::ImageOverview => ready(
                    ticket.clone(),
                    DetailRefreshResult::ImageOverview(self.image_properties(name)),
                ),
                DetailPane::ImageConfig => job(
                    ticket.clone(),
                    DetailRefreshWork::ImageConfig { name: name.clone() },
                ),
                DetailPane::ImageUnit => job(
                    ticket.clone(),
                    DetailRefreshWork::ImageUnit { name: name.clone() },
                ),
                DetailPane::Properties
                | DetailPane::Details
                | DetailPane::Logs
                | DetailPane::Config
                | DetailPane::Metrics => ready(ticket.clone(), DetailRefreshResult::Noop),
            },
        };
        Some(prepared)
    }

    pub(crate) fn apply_detail_refresh(&mut self, completion: DetailRefreshCompletion) {
        if !self.data.detail_refresh.finish(completion.ticket.revision) {
            return;
        }
        if completion.ticket.target != self.data.detail_target
            || completion.ticket.pane != self.ui.detail_panel.active_pane()
        {
            return;
        }

        match completion.result {
            DetailRefreshResult::Empty => {
                self.data.properties = Ok(MachineProperties::default());
                self.data.properties_dirty = true;
                self.data.details_dirty = true;
                self.data.log_manager.cleanup_all();
                self.apply_config_snapshot(None);
                self.data.unit_name = None;
                self.data.unit_drop_ins.clear();
                self.data.unit_dirty = true;
            }
            DetailRefreshResult::Noop => {}
            DetailRefreshResult::MachineProperties(result) => {
                match result {
                    Ok(query) => {
                        if let Some(fallback) = query.fallback {
                            self.set_status(
                                format!(
                                    "{} unavailable: {}; using {}",
                                    fallback.from.label(),
                                    fallback.reason,
                                    fallback.to.label()
                                ),
                                crate::tui::StatusLevel::Warn,
                            );
                        }
                        self.data.properties = Ok(query.value);
                    }
                    Err(error) => {
                        log::debug!("{error}");
                        self.data.properties = Err(error);
                    }
                }
                self.data.properties_dirty = true;
                self.data.details_dirty = true;
            }
            DetailRefreshResult::Journal(mut result) => {
                let Some(name) = completion.ticket.target.name() else {
                    return;
                };
                if let Some(handle) = result.handle.take() {
                    self.data.log_manager.attach_stream(name, handle);
                } else if let Some(error) = result.error {
                    self.data
                        .log_manager
                        .push_line(name, format!("Log stream error: {error}"));
                    if let Some(hint) = result.hint {
                        self.data.log_manager.push_line(name, hint);
                    }
                    self.data.log_manager.mark_stream_failed(name);
                }
            }
            DetailRefreshResult::Config(result) => {
                let name = completion.ticket.target.name().unwrap_or_default();
                if let Err(error) = &result {
                    if error.is_unsupported() {
                        log::debug!("No nspawn config route for {name}: {error}");
                    } else {
                        log::warn!("Failed to read .nspawn config for {name}: {error}");
                    }
                }
                self.apply_config_result(result);
            }
            DetailRefreshResult::ImageOverview(properties) => {
                self.data.properties = Ok(properties);
                self.data.properties_dirty = true;
                self.data.details_dirty = true;
            }
            DetailRefreshResult::ImageUnit(result) => {
                self.data.properties = match result.properties {
                    Ok(Some(properties)) => Ok(properties),
                    Ok(None) => Ok(MachineProperties::default()),
                    Err(error) => Err(error.to_string()),
                };
                self.data.properties_dirty = true;
                self.data.details_dirty = true;
                match result.unit {
                    Some(Ok(unit)) => {
                        self.data.unit_name = Some(unit.unit);
                        self.data.unit_drop_ins = unit.drop_ins;
                    }
                    Some(Err(error)) => {
                        let name = completion.ticket.target.name().unwrap_or_default();
                        log::debug!("Failed to read unit drop-ins for {name}: {error}");
                        self.data.unit_name = MachineName::new(name)
                            .ok()
                            .map(|name| name.systemd_nspawn_unit());
                        self.data.unit_drop_ins.clear();
                    }
                    None => {
                        self.data.unit_name = None;
                        self.data.unit_drop_ins.clear();
                    }
                }
                self.data.unit_dirty = true;
            }
        }
    }

    fn apply_config_snapshot(
        &mut self,
        config: Option<crate::application::inspection::NspawnConfigInspection>,
    ) {
        self.apply_config_result(Ok(config));
    }

    fn apply_config_result(
        &mut self,
        result: Result<
            Option<crate::application::inspection::NspawnConfigInspection>,
            crate::application::inspection::ResourceInspectionError,
        >,
    ) {
        let (config, error) = match result {
            Ok(config) => (config, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let new_path = config.as_ref().map(|config| config.path.clone());
        let new_content = config.map(|config| config.content);
        if self.data.config_content != new_content || self.data.config_error != error {
            self.ui.detail_panel.config_scroll = 0;
            self.data.config_dirty = true;
        }
        self.data.config_path = new_path;
        self.data.config_content = new_content;
        self.data.config_error = error;
    }

    fn detail_refresh_services(&self) -> DetailRefreshServices {
        DetailRefreshServices {
            runtime_catalog: self.data.runtime_catalog.clone(),
            session_service: self.data.session_service.clone(),
            resource_inspection: self.data.resource_inspection.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn refresh_detail_now(&mut self) {
        self.request_detail_refresh();
        let Some(prepared) = self.prepare_detail_refresh() else {
            return;
        };
        let completion = match prepared {
            PreparedDetailRefresh::Ready(completion) => completion,
            PreparedDetailRefresh::Job(job) => job.execute(self.detail_refresh_services()).await,
        };
        self.apply_detail_refresh(completion);
    }

    fn image_properties(&self, name: &str) -> MachineProperties {
        let mut properties = MachineProperties::from_inspection(
            InspectionSource::RuntimeState,
            InspectionCompleteness::RuntimeOnly,
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

    pub fn set_status(&mut self, msg: String, level: crate::tui::StatusLevel) {
        self.set_status_for(msg, level, Duration::from_secs(4));
    }

    pub fn set_status_for(
        &mut self,
        msg: String,
        level: crate::tui::StatusLevel,
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
        self.ui.focus.is_image_list()
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
        if !self.ui.focus.is_inspector() {
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

    pub(crate) fn focused_machine_resource(&self) -> Option<&crate::domain::runtime::MachineEntry> {
        if self.ui.focus.is_machine_list() {
            return self.data.entries.get(self.data.selected);
        }
        if !self.ui.focus.is_inspector() {
            return None;
        }
        let DetailTarget::Machine(name) = &self.data.detail_target else {
            return None;
        };
        self.data.entries.iter().find(|entry| &entry.name == name)
    }

    pub(crate) fn image_has_running_machine(&self, image: &ImageEntry) -> bool {
        !image.is_hidden()
            && self
                .data
                .entries
                .iter()
                .any(|entry| entry.name == image.name && entry.state == MachineState::Running)
    }

    fn submit_machine_operation(
        &mut self,
        name: String,
        action: MachineLifecycleAction,
        operation: MachineOperation,
    ) -> bool {
        let tx = match &self.ui.app_tx {
            Some(tx) => tx.clone(),
            None => return false,
        };
        self.apply_machine_projection();

        let pm = self.permissions.clone();
        let host_operation = self.data.host_operations.begin();
        tokio::spawn(async move {
            let _host_operation = host_operation;
            let audit = match pm
                .request_elevation(format!("{} {}", action.audit_label(), name))
                .await
            {
                Ok(audit) => audit,
                Err(error) => {
                    drop(operation);
                    let _ = tx
                        .send(crate::tui::events::AppEvent::MachineLifecycleFinished(
                            crate::application::MachineLifecycleOutcome {
                                machine: MachineName::new(name)
                                    .expect("lifecycle operation already validated the name"),
                                action,
                                result: crate::application::MachineLifecycleResult::NotAttempted(
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
            let outcome = audit.run(async move { operation.run().await }).await;
            let _ = tx
                .send(crate::tui::events::AppEvent::MachineLifecycleFinished(
                    outcome,
                ))
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
        if let Some(name) = self
            .focused_image_resource()
            .map(|image| image.name.clone())
        {
            self.action_start_image_named(&name);
            return;
        }
        self.set_status(
            "Focus Images to start an image.".into(),
            crate::tui::StatusLevel::Info,
        );
    }

    pub fn action_poweroff(&mut self) {
        if let Some(name) = self
            .focused_machine_resource()
            .map(|entry| entry.name.clone())
        {
            self.action_runtime_named(&name, MachineRuntimeAction::Poweroff);
            return;
        }
        self.set_status(
            "Focus Machines to power off a running machine.".into(),
            crate::tui::StatusLevel::Info,
        );
    }

    pub(crate) fn action_runtime_named(&mut self, name: &str, action: MachineRuntimeAction) {
        let Some(entry) = self
            .data
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
        else {
            self.set_status(
                format!("Machine '{}' changed before the action started.", name),
                crate::tui::StatusLevel::Warn,
            );
            return;
        };
        let name = entry.name.clone();
        match self.data.machine_lifecycle.begin_runtime(&entry, action) {
            Ok(operation) => {
                self.submit_machine_operation(
                    name,
                    MachineLifecycleAction::Runtime(action),
                    operation,
                );
            }
            Err(rejection) => self.set_status(
                format!("{}: {}", name, rejection),
                crate::tui::StatusLevel::Warn,
            ),
        }
    }

    pub fn action_remove(&mut self) {
        if self.ui.delete_dialog.is_some() {
            self.action_remove_image();
            return;
        }
        self.set_status(
            "Focus Images to remove a machine image.".into(),
            crate::tui::StatusLevel::Info,
        );
    }

    pub(crate) fn action_start_image_named(&mut self, name: &str) {
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
                    format!("Image '{}' changed before the action started.", name),
                    crate::tui::StatusLevel::Warn,
                );
                return;
            }
        };
        if image.is_hidden() {
            self.set_status(
                "Internal images cannot be started directly.".into(),
                crate::tui::StatusLevel::Warn,
            );
            return;
        }
        let name = image.name.clone();
        let is_mstack = image.image_type == "mstack";
        let observed_state = self
            .data
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.state.clone());
        if let Some(state) = &observed_state {
            self.set_status(
                format!("{} is already {}.", name, state.label()),
                crate::tui::StatusLevel::Info,
            );
            return;
        }
        let operation = match self
            .data
            .machine_lifecycle
            .begin_launch(&image, observed_state)
        {
            Ok(operation) => operation,
            Err(rejection) => {
                self.set_status(
                    format!("{}: {}", name, rejection),
                    crate::tui::StatusLevel::Warn,
                );
                return;
            }
        };
        if self.submit_machine_operation(name.clone(), MachineLifecycleAction::Launch, operation)
            && is_mstack
        {
            self.set_status(
                format!(
                    "{} is an OCI application and may not be bootable; systemd will still try to start it.",
                    name
                ),
                crate::tui::StatusLevel::Warn,
            );
        }
    }

    fn action_remove_image(&mut self) {
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
                crate::tui::StatusLevel::Warn,
            );
            return;
        }
        if self.data.entries.iter().any(|machine| machine.name == name) {
            self.set_status(
                format!("Stop machine '{}' before deleting its image.", name),
                crate::tui::StatusLevel::Warn,
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
                    crate::tui::StatusLevel::Warn,
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
                self.set_status(error.to_string(), crate::tui::StatusLevel::Warn);
                return;
            }
        };
        let tx = match &self.ui.app_tx {
            Some(tx) => tx.clone(),
            None => return,
        };
        let pm = self.permissions.clone();
        let operation = self.data.host_operations.begin();
        tokio::spawn(async move {
            let _operation = operation;
            let audit = match pm.request_elevation(format!("Remove image {}", name)).await {
                Ok(audit) => audit,
                Err(error) => {
                    let _ = tx
                        .send(crate::tui::events::AppEvent::ActionDone(
                            format!("Remove failed: {}", error),
                            crate::tui::StatusLevel::Error,
                        ))
                        .await;
                    return;
                }
            };
            let result = audit.run(async { removal.run().await }).await;
            let event = match result {
                crate::application::ImageRemovalOutcome::NotAttempted { reason, .. } => {
                    crate::tui::events::AppEvent::ActionDone(
                        format!("Remove was not attempted: {}", reason),
                        crate::tui::StatusLevel::Warn,
                    )
                }
                crate::application::ImageRemovalOutcome::Removed(report) => {
                    let unit_warning = match &report.unit {
                        crate::application::image_lifecycle::UnitDisableReport::Failed(reason) => {
                            Some(reason.clone())
                        }
                        _ => None,
                    };
                    match report.artifacts {
                        crate::application::image_lifecycle::ArtifactCleanupReport::Removed => {
                            match unit_warning {
                                Some(reason) => crate::tui::events::AppEvent::ActionDone(
                                    format!(
                                        "Removed image {} and Lasper artifacts; unit disable warning: {}",
                                        name, reason
                                    ),
                                    crate::tui::StatusLevel::Warn,
                                ),
                                None => crate::tui::events::AppEvent::ActionDone(
                                    format!("Removed image {} and Lasper artifacts", name),
                                    crate::tui::StatusLevel::Success,
                                ),
                            }
                        }
                        crate::application::image_lifecycle::ArtifactCleanupReport::PreservedAmbiguous(
                            errors,
                        )
                        | crate::application::image_lifecycle::ArtifactCleanupReport::PartiallyRemoved(errors)
                        | crate::application::image_lifecycle::ArtifactCleanupReport::Failed(
                            errors,
                        ) => crate::tui::events::AppEvent::ActionDone(
                            format!(
                                "Removed image {}; cleanup warning: {}{}",
                                name,
                                errors.join("; "),
                                unit_warning
                                    .as_deref()
                                    .map(|reason| format!("; unit disable: {reason}"))
                                    .unwrap_or_default()
                            ),
                            crate::tui::StatusLevel::Warn,
                        ),
                        _ => match unit_warning {
                            Some(reason) => crate::tui::events::AppEvent::ActionDone(
                                format!("Removed image {}; unit disable warning: {}", name, reason),
                                crate::tui::StatusLevel::Warn,
                            ),
                            None => crate::tui::events::AppEvent::ActionDone(
                                format!("Removed image {}", name),
                                crate::tui::StatusLevel::Success,
                            ),
                        },
                    }
                }
                crate::application::ImageRemovalOutcome::Rejected { reason, .. } => {
                    crate::tui::events::AppEvent::ActionDone(
                        format!("Remove rejected: {}", reason),
                        crate::tui::StatusLevel::Warn,
                    )
                }
                crate::application::ImageRemovalOutcome::Failed { reason, .. } => {
                    crate::tui::events::AppEvent::ActionDone(
                        format!("Remove failed: {}", reason),
                        crate::tui::StatusLevel::Error,
                    )
                }
                crate::application::ImageRemovalOutcome::OutcomeUnknown { reason, .. } => {
                    crate::tui::events::AppEvent::ActionDone(
                        format!("Removal outcome unknown: {}", reason),
                        crate::tui::StatusLevel::Warn,
                    )
                }
            };
            let _ = tx.send(event).await;
        });
    }

    pub(crate) fn action_unit_named(&mut self, name: &str, action: NspawnUnitAction) {
        let Some(image) = self
            .data
            .images
            .iter()
            .find(|image| image.name == name)
            .cloned()
        else {
            self.set_status(
                format!("Image '{}' changed before the action started.", name),
                crate::tui::StatusLevel::Warn,
            );
            return;
        };
        let name = image.name.clone();
        match self.data.machine_lifecycle.begin_unit(&image, action) {
            Ok(operation) => {
                self.submit_machine_operation(
                    name,
                    MachineLifecycleAction::Unit(action),
                    operation,
                );
            }
            Err(rejection) => self.set_status(
                format!("{}: {}", name, rejection),
                crate::tui::StatusLevel::Warn,
            ),
        }
    }

    pub async fn spawn_terminal(&mut self) {
        let entry = if self.focused_image_resource().is_some() {
            let Some(image) = self.focused_image_resource().cloned() else {
                self.set_status("No image selected.".into(), crate::tui::StatusLevel::Warn);
                return;
            };
            if image.is_hidden() {
                self.set_status(
                    "Internal images do not provide terminal sessions.".into(),
                    crate::tui::StatusLevel::Info,
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
                    crate::tui::StatusLevel::Info,
                );
                return;
            };
            let machine = self.data.entries[machine_idx].clone();
            if machine.state != MachineState::Running {
                self.set_status(
                    format!(
                        "{} is {} and cannot accept a terminal.",
                        image.name,
                        machine.state.label()
                    ),
                    crate::tui::StatusLevel::Info,
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
        if !self.ui.focus.is_terminal() {
            self.ui.prev_focus = self.ui.focus;
        }
        let rows = self.ui.pane_height.max(10);

        match self
            .data
            .terminal
            .spawn(&entry, rows, &self.ui.app_tx)
            .await
        {
            Ok(session) => {
                self.set_focus(crate::tui::app::WorkspaceFocus::Terminal);
                self.request_detail_refresh();
                let message = match session.attach_kind {
                    crate::domain::session::TerminalAttachmentKind::Login => {
                        format!("Logged into {}", entry.name)
                    }
                    crate::domain::session::TerminalAttachmentKind::Namespace => {
                        format!("Attached to {} through its namespaces", entry.name)
                    }
                };
                self.set_status(message, crate::tui::StatusLevel::Info);
            }
            Err(msg) => {
                self.set_status(msg, crate::tui::StatusLevel::Error);
            }
        }
    }

    pub fn sync_terminal_to_selected(&mut self) {
        let was_focused = self.ui.focus.is_terminal();
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
