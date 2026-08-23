use super::{App, ModalLayer, WorkspaceFocus};
use crate::tui::core::{AppMessage, Component, ContainerMessage, EventResult, ListMessage};
use crate::tui::wizard::StepAction as WizardAction;
use crate::tui::StatusLevel;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

async fn contain_wizard_task<T>(
    label: &'static str,
    future: impl std::future::Future<
        Output = Result<T, crate::application::provisioning::DeploymentError>,
    >,
) -> Result<T, crate::application::provisioning::DeploymentError> {
    use futures_util::FutureExt as _;

    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            let message = format!("{label} panicked: {detail}");
            log::error!("{message}");
            Err(crate::application::provisioning::DeploymentError::failed(
                message,
            ))
        }
    }
}

fn quit_confirmation_message(terminal_sessions: usize, host_operations: usize) -> Option<String> {
    if terminal_sessions == 0 && host_operations == 0 {
        return None;
    }

    let mut warnings = Vec::new();
    if terminal_sessions > 0 {
        warnings.push(format!(
            "{terminal_sessions} active terminal session{} will be terminated.",
            if terminal_sessions == 1 { "" } else { "s" }
        ));
    }
    if host_operations > 0 {
        warnings.push(format!(
            "{host_operations} host operation{} still running. Quitting now may interrupt {} and leave partial host changes.",
            if host_operations == 1 { " is" } else { "s are" },
            if host_operations == 1 { "it" } else { "them" }
        ));
    }
    warnings.push("Quit anyway?".to_string());
    Some(warnings.join("\n"))
}

// Top-level dispatch
//
// handle_key is now a thin chain of mode-specific handlers.  Each handler
// returns `true` when it consumed the key — the remaining handlers are
// skipped.  This replaces the previous 300-line nested match.

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
        // The modal layer is shared with mouse dispatch.  It must receive
        // input before terminal and workspace shortcuts regardless of the
        // focus that was active when it opened.
        if self.handle_modal_key(key).await {
            return;
        }

        // Layer 1.5 – resize mode (skip when terminal is in insert mode)
        if self.ui.resize_mode == super::ResizeMode::Active
            && !self.is_terminal_insert_mode()
            && self.handle_resize_key(key)
        {
            return;
        }

        // Layer 2 – terminal panel when it owns focus
        if self.ui.focus.is_terminal() && self.handle_terminal_focused_key(key).await {
            return;
        }

        // Layer 4 – global shortcuts
        if self.handle_global_key(key).await {
            return;
        }

        // Layer 5 – route to the focused panel
        self.route_to_focused_panel(key).await;
    }
}

// Mouse dispatch

impl App {
    pub async fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.handle_modal_mouse(mouse) {
            return;
        }

        // Hit-test: which panel is the mouse over?
        let layout = self.ui.panel_layout;
        let col = mouse.column;
        let row = mouse.row;

        let maximized = self.data.terminal.is_showing() && self.data.terminal.maximized;

        let hit = if in_rect(col, row, layout.machines) {
            Some(WorkspaceFocus::Machines)
        } else if in_rect(col, row, layout.images) {
            Some(WorkspaceFocus::Images)
        } else if !maximized && in_rect(col, row, layout.detail) {
            Some(WorkspaceFocus::for_panel(self.ui.focus, 2))
        } else if layout.terminal.is_some_and(|r| in_rect(col, row, r)) {
            Some(WorkspaceFocus::Terminal)
        } else {
            None
        };

        // Click-to-focus on button press.
        let mut focus_changed = false;
        if let (Some(focus), MouseEventKind::Down(_)) = (hit, mouse.kind) {
            if !(self.data.terminal.maximized
                && self.data.terminal.is_showing()
                && focus.is_inspector())
            {
                focus_changed = self.ui.focus != focus;
                self.set_focus(focus);
            }
        }

        if !maximized && in_rect(col, row, layout.detail) {
            let _ = self.ui.detail_panel.handle_mouse(mouse);
        }

        if focus_changed {
            self.request_detail_refresh();
        }

        // Terminal panel: forward mouse to PTY in insert mode, scroll in normal mode.
        if self.ui.focus.is_terminal()
            && self.data.terminal.is_showing()
            && (layout.terminal.is_some_and(|r| in_rect(col, row, r))
                || self.data.terminal.wants_mouse_capture())
        {
            match self.data.terminal.handle_mouse(mouse) {
                crate::tui::views::terminal_panel::TerminalInputStatus::Queued => {}
                crate::tui::views::terminal_panel::TerminalInputStatus::Full => self.set_status(
                    "Terminal input queue is full; input was dropped.".into(),
                    StatusLevel::Warn,
                ),
                crate::tui::views::terminal_panel::TerminalInputStatus::Closed => self.set_status(
                    "Terminal input channel is closed.".into(),
                    StatusLevel::Error,
                ),
            }
        }
    }
}

impl App {
    async fn handle_modal_key(&mut self, key: KeyEvent) -> bool {
        match self.ui.modal_layer() {
            Some(ModalLayer::Dialog) => self.handle_dialog_key(key).await,
            Some(ModalLayer::DeleteConfirmation) => self.handle_delete_confirm_key(key),
            Some(ModalLayer::QuitConfirmation) => self.handle_quit_confirm_key(key),
            Some(ModalLayer::Wizard | ModalLayer::Help | ModalLayer::PowerMenu) => {
                self.handle_overlay_key(key).await
            }
            None => false,
        }
    }
}

fn in_rect(col: u16, row: u16, r: ratatui::layout::Rect) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

// Layer 0: delete confirmation

impl App {
    fn handle_delete_confirm_key(&mut self, key: KeyEvent) -> bool {
        if self.ui.delete_dialog.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.action_remove();
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.ui.delete_dialog = None;
            }
            _ => {
                if let Some(dialog) = &mut self.ui.delete_dialog {
                    let _ = dialog.handle_key(key);
                }
            }
        }
        true
    }
}

// Layer 1: quit confirmation

impl App {
    fn handle_quit_confirm_key(&mut self, key: KeyEvent) -> bool {
        if self.ui.quit_dialog.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.data.terminal.cleanup_all();
                self.signal_quit();
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.ui.quit_dialog = None;
            }
            _ => {}
        }
        true
    }
}

// Layer 2: terminal panel (insert / normal mode + tab switching)

impl App {
    async fn handle_terminal_focused_key(&mut self, key: KeyEvent) -> bool {
        use crate::tui::views::terminal_panel::TerminalKeyOutcome;

        let outcome =
            self.data
                .terminal
                .handle_key(key, &self.data.entries, &mut self.data.selected);

        // If closing a tab emptied the terminal panel, restore focus to a valid panel.
        let restored_focus = self.ui.focus.is_terminal() && !self.data.terminal.is_showing();
        if restored_focus {
            self.restore_non_terminal_focus();
        }

        if restored_focus && !matches!(&outcome, TerminalKeyOutcome::ConsumedAndRefreshDetail) {
            self.request_detail_refresh();
        }

        match outcome {
            TerminalKeyOutcome::Consumed => true,
            TerminalKeyOutcome::PassThrough => false,
            TerminalKeyOutcome::ConsumedAndRefreshDetail => {
                self.request_detail_refresh();
                true
            }
            TerminalKeyOutcome::InputQueueFull => {
                self.set_status(
                    "Terminal input queue is full; input was dropped.".into(),
                    StatusLevel::Warn,
                );
                true
            }
            TerminalKeyOutcome::InputChannelClosed => {
                self.set_status(
                    "Terminal input channel is closed.".into(),
                    StatusLevel::Error,
                );
                true
            }
        }
    }
}

// Layer 3: overlays (wizard, help, power menu)

impl App {
    async fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        // 3a – wizard
        if self.ui.show_wizard {
            if let Some(action) = self.ui.wizard.as_mut().map(|wizard| wizard.handle_key(key)) {
                self.handle_wizard_action(action).await;
            } else {
                self.ui.show_wizard = false;
            }
            return true;
        }

        // 3b – help (any key dismisses)
        if self.ui.show_help {
            self.ui.show_help = false;
            return true;
        }

        // 3c – power menu
        if let Some(pm) = &mut self.ui.power_menu {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.ui.power_menu = None,
                KeyCode::Enter => {
                    let idx = pm.get_selected();
                    let image_menu = pm.is_image_menu();
                    self.ui.power_menu = None;
                    if image_menu {
                        match idx {
                            0 => self.action_start(),
                            1 => self.show_delete_dialog(),
                            _ => {}
                        }
                    } else {
                        match idx {
                            0 => self.action_start(),
                            1 => self.action_poweroff(),
                            2 => self.action_reboot(),
                            3 => self.action_terminate(),
                            4 => self.action_kill(),
                            5 => self.action_enable(),
                            6 => self.action_disable(),
                            _ => {}
                        }
                    }
                }
                _ => {
                    let _ = pm.handle_key(key);
                }
            }
            return true;
        }

        false
    }
}

// Layer 4: global shortcuts

impl App {
    fn request_quit(&mut self) {
        let message = quit_confirmation_message(
            self.data.terminal.sessions.len(),
            self.data.host_operations.active_count(),
        );
        if let Some(message) = message {
            self.ui.quit_dialog = Some(
                crate::tui::widgets::dialogs::confirmation::ConfirmationDialog::new(
                    "Quit Lasper?",
                    message,
                ),
            );
        } else {
            self.signal_quit();
        }
    }

    async fn handle_global_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_quit();
                true
            }
            KeyCode::Char('?') => {
                self.ui.show_help = true;
                true
            }
            KeyCode::Tab => {
                self.cycle_main_focus(true);
                self.request_detail_refresh();
                true
            }
            KeyCode::BackTab => {
                self.cycle_main_focus(false);
                self.request_detail_refresh();
                true
            }
            KeyCode::Char('s') => {
                self.action_start();
                true
            }
            KeyCode::Char('S') => {
                self.action_poweroff();
                true
            }
            KeyCode::Char('x') | KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if self.image_is_focused() && self.ui.image_list.shows_internal() {
                    self.set_status(
                        "Internal images do not expose a start/action menu.".into(),
                        crate::tui::StatusLevel::Info,
                    );
                    return true;
                }
                let has_selection = if self.image_is_focused() {
                    !self.active_images().0.is_empty()
                } else {
                    !self.data.entries.is_empty()
                };
                if has_selection {
                    self.ui.power_menu = Some(if self.image_is_focused() {
                        crate::tui::widgets::power_menu::PowerMenu::new_for_images(0)
                    } else {
                        crate::tui::widgets::power_menu::PowerMenu::new(0)
                    });
                }
                true
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                self.begin_wizard().await;
                true
            }
            KeyCode::Char('r') => {
                self.refresh().await;
                true
            }
            KeyCode::Char('R') => {
                self.ui.resize_mode = if self.ui.resize_mode == super::ResizeMode::Active {
                    super::ResizeMode::Inactive
                } else {
                    super::ResizeMode::Active
                };
                true
            }
            KeyCode::Char('t') => {
                self.toggle_terminal().await;
                true
            }
            KeyCode::Char('T') => {
                if self.data.terminal.is_showing() {
                    self.data.terminal.maximized = !self.data.terminal.maximized;
                    if self.data.terminal.maximized && self.ui.focus.is_inspector() {
                        self.set_focus(WorkspaceFocus::Terminal);
                    }
                }
                true
            }
            KeyCode::Char('D') => {
                self.show_delete_dialog();
                true
            }
            _ => false,
        }
    }
}

// Layer 5: route to focused panel

impl App {
    async fn route_to_focused_panel(&mut self, key: KeyEvent) {
        match self.ui.focus {
            WorkspaceFocus::Machines => {
                let result = self
                    .ui
                    .container_list
                    .handle_key(key, self.data.entries.len());
                self.handle_container_list_result(result).await;
            }
            WorkspaceFocus::Images => {
                let was_internal = self.ui.image_list.shows_internal();
                let image_count = self.active_images().0.len();
                let result = self.ui.image_list.handle_key(key, image_count);
                if was_internal != self.ui.image_list.shows_internal() {
                    self.update_detail_target();
                    self.request_detail_refresh();
                }
                self.handle_container_list_result(result).await;
            }
            WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector => {
                let target = self.data.detail_target.clone();
                let result = self.ui.detail_panel.handle_key(key, &target);
                self.handle_detail_panel_result(result).await;
            }
            WorkspaceFocus::Terminal => {
                // Already handled in layer 2; only reached when there are
                // no active sessions (empty terminal panel).
            }
        }
    }

    async fn handle_container_list_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::List(ListMessage::Next)) => {
                self.select_next();
                if self.image_is_focused() {
                    self.update_detail_target();
                    self.request_detail_refresh();
                } else {
                    self.sync_terminal_to_selected();
                    self.request_detail_refresh();
                }
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.select_prev();
                if self.image_is_focused() {
                    self.update_detail_target();
                    self.request_detail_refresh();
                } else {
                    self.sync_terminal_to_selected();
                    self.request_detail_refresh();
                }
            }
            _ => {}
        }
    }

    async fn handle_detail_panel_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(_pane))) => {
                self.request_detail_refresh();
            }
            EventResult::Consumed | EventResult::Ignored => {}
            _ => {}
        }
    }
}

// Composite actions

impl App {
    async fn begin_wizard(&mut self) {
        if !self.permissions.level().is_elevated() {
            self.set_status("Root required - run: lasper -e".into(), StatusLevel::Error);
            return;
        }

        let host = match contain_wizard_task(
            "provisioning host inspection",
            self.data.provisioning_preparation.inspect_host(),
        )
        .await
        {
            Ok(host) => host,
            Err(error) => {
                self.set_status(
                    format!("Could not inspect provisioning capabilities: {error}"),
                    StatusLevel::Error,
                );
                return;
            }
        };
        let wizard_id = crate::tui::wizard::WizardInstanceId::new(self.ui.next_wizard_instance);
        self.ui.next_wizard_instance = self.ui.next_wizard_instance.checked_add(1).unwrap_or(1);
        let mut wizard = crate::tui::wizard::Wizard::new(
            wizard_id,
            self.data.entries.clone(),
            self.data.images.clone(),
            self.permissions.level(),
            self.config.clone(),
            host,
            self.data.provisioning_preparation.clone(),
        );

        if let Some(tx) = self.ui.app_tx.clone() {
            let preparation = self.data.provisioning_preparation.clone();
            tokio::spawn(async move {
                let result = contain_wizard_task(
                    "provisioning hardware discovery",
                    preparation.discover_hardware(),
                )
                .await;
                let _ = tx
                    .send(
                        crate::tui::events::AppEvent::WizardHardwareDiscoveryFinished {
                            wizard_id,
                            result,
                        },
                    )
                    .await;
            });
        } else {
            wizard.draft.passthrough.hardware_scanning = false;
        }

        self.ui.wizard = Some(wizard);
        self.ui.show_wizard = true;
    }

    pub(crate) async fn handle_wizard_action(&mut self, action: WizardAction) {
        match action {
            WizardAction::None | WizardAction::Next | WizardAction::Prev => {}
            WizardAction::Close => {
                self.ui.pending_deployment_preflight = None;
                self.ui.show_wizard = false;
                self.ui.wizard = None;
            }
            WizardAction::Status(message, level) => self.set_status(message, level),
            WizardAction::OpenDialog(dialog) => self.ui.active_dialog = Some(dialog),
            WizardAction::CloseDialog => self.ui.active_dialog = None,
            WizardAction::ValidateInterface {
                name,
                is_bridge_mode,
            } => {
                let Some(tx) = self.ui.app_tx.clone() else {
                    if let Some(wizard) = &mut self.ui.wizard {
                        wizard.loading = false;
                    }
                    self.set_status(
                        "Internal error: application event channel is unavailable".into(),
                        StatusLevel::Error,
                    );
                    return;
                };
                let Some(wizard_id) = self.ui.wizard.as_ref().map(|wizard| wizard.id()) else {
                    return;
                };
                let preparation = self.data.provisioning_preparation.clone();
                tokio::spawn(async move {
                    let result = contain_wizard_task(
                        "provisioning interface validation",
                        preparation.validate_interface(&name, is_bridge_mode),
                    )
                    .await;
                    let _ = tx
                        .send(
                            crate::tui::events::AppEvent::WizardInterfaceValidationFinished {
                                wizard_id,
                                result,
                            },
                        )
                        .await;
                });
            }
            WizardAction::PreflightDeployment(request) => {
                self.ui.active_dialog = None;
                let Some(tx) = self.ui.app_tx.clone() else {
                    if let Some(wizard) = &mut self.ui.wizard {
                        wizard.preflight_dispatch_failed();
                    }
                    self.set_status(
                        "Internal error: application event channel is unavailable".into(),
                        StatusLevel::Error,
                    );
                    return;
                };
                let preflight_id = self.ui.next_deployment_preflight;
                self.ui.next_deployment_preflight = self
                    .ui
                    .next_deployment_preflight
                    .checked_add(1)
                    .unwrap_or(1);
                self.ui.pending_deployment_preflight = Some(preflight_id);
                let service = self.data.provisioning.clone();
                tokio::spawn(async move {
                    let result = service.preflight(&request).await;
                    let _ = tx
                        .send(crate::tui::events::AppEvent::DeploymentPreflightFinished {
                            preflight_id,
                            request,
                            result,
                        })
                        .await;
                });
            }
            WizardAction::StartDeployment(submission) => {
                self.ui.pending_deployment_preflight = None;
                self.ui.active_dialog = None;
                match self.data.provisioning.start(submission) {
                    Ok(handle) => {
                        log::info!("[DEPLOY] accepted application job {}", handle.id());
                        if let Some(wizard) = &mut self.ui.wizard {
                            wizard.start_deployment(handle);
                        }
                    }
                    Err(error) => {
                        if let Some(wizard) = &mut self.ui.wizard {
                            wizard.loading = false;
                        }
                        self.set_status(
                            format!("Deployment rejected: {error}"),
                            StatusLevel::Error,
                        );
                    }
                }
            }
            WizardAction::ReleaseUnresolvedDeployment(deployment_id) => {
                let Some(tx) = self.ui.app_tx.clone() else {
                    self.set_status(
                        "Internal error: application event channel is unavailable".into(),
                        StatusLevel::Error,
                    );
                    return;
                };
                let Some(wizard_id) = self.ui.wizard.as_ref().map(|wizard| wizard.id()) else {
                    return;
                };
                let service = self.data.provisioning.clone();
                tokio::spawn(async move {
                    let result = service.release_unresolved(deployment_id, true).await;
                    let _ = tx
                        .send(
                            crate::tui::events::AppEvent::DeploymentClaimReleaseFinished {
                                wizard_id,
                                deployment_id,
                                result,
                            },
                        )
                        .await;
                });
            }
        }
    }

    async fn toggle_terminal(&mut self) {
        if self.data.terminal.is_showing() {
            self.data.terminal.show = false;
            if self.ui.focus.is_terminal() {
                self.restore_non_terminal_focus();
            }
            self.request_detail_refresh();
        } else {
            self.spawn_terminal().await;
        }
    }

    pub(super) fn show_delete_dialog(&mut self) {
        if self.image_is_focused() {
            let image = match self.selected_image() {
                Some(image) => image,
                None => return,
            };
            if crate::nspawn::models::ImageEntry::is_protected_name(&image.name) {
                self.set_status(
                    "The .host image cannot be removed.".into(),
                    crate::tui::StatusLevel::Warn,
                );
                return;
            }
            if self
                .data
                .entries
                .iter()
                .any(|machine| machine.name == image.name && machine.state.is_running())
            {
                self.set_status(
                    format!("Stop machine '{}' before deleting its image.", image.name),
                    crate::tui::StatusLevel::Warn,
                );
                return;
            }
            let detail = if image.is_hidden() {
                "This hidden image will be removed through systemd's image management path."
            } else if image.readonly {
                "This read-only image and its local data will be removed."
            } else {
                "This image and its local data will be removed."
            };
            let target = match crate::nspawn::models::ImageName::new(image.name.clone()) {
                Ok(target) => target,
                Err(error) => {
                    self.set_status(error.to_string(), crate::tui::StatusLevel::Error);
                    return;
                }
            };
            let cleanup_supported =
                !image.is_hidden() && crate::nspawn::models::MachineName::new(&image.name).is_ok();
            let mut dialog = crate::tui::widgets::dialogs::confirmation::ConfirmationDialog::new(
                "Delete Image",
                format!(
                    "Delete '{}'?\n{}\nsystemd also attempts to remove all same-name .nspawn settings.",
                    image.name, detail
                ),
            );
            if cleanup_supported {
                dialog = dialog.with_checkbox("Remove Lasper NVIDIA state and unit drop-ins", true);
            }
            self.ui.delete_dialog = Some(super::PendingImageRemoval::new(target, dialog));
            return;
        }
        self.set_status(
            "Focus Images to remove a machine image.".into(),
            crate::tui::StatusLevel::Info,
        );
    }
}

// Resize mode

impl App {
    fn is_terminal_insert_mode(&self) -> bool {
        self.ui.focus.is_terminal()
            && self
                .data
                .terminal
                .active_session()
                .map(|session| session.is_insert_mode())
                .unwrap_or(false)
    }

    fn handle_resize_key(&mut self, key: KeyEvent) -> bool {
        let percentage_step = 5u16;

        match key.code {
            KeyCode::Esc | KeyCode::Char('R') | KeyCode::Char('q') => {
                self.ui.resize_mode = super::ResizeMode::Inactive;
                true
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.ui.container_list_pct = self
                    .ui
                    .container_list_pct
                    .saturating_sub(percentage_step)
                    .max(super::CONTAINER_LIST_PCT_MIN);
                true
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.ui.container_list_pct = self
                    .ui
                    .container_list_pct
                    .saturating_add(percentage_step)
                    .min(super::CONTAINER_LIST_PCT_MAX);
                true
            }
            KeyCode::Char('j') | KeyCode::Down
                if self.ui.focus.is_machine_list() || self.ui.focus.is_image_list() =>
            {
                self.ui.left_machines_pct = self
                    .ui
                    .left_machines_pct
                    .saturating_add(percentage_step)
                    .min(super::LEFT_MACHINES_PCT_MAX);
                true
            }
            KeyCode::Char('k') | KeyCode::Up
                if self.ui.focus.is_machine_list() || self.ui.focus.is_image_list() =>
            {
                self.ui.left_machines_pct = self
                    .ui
                    .left_machines_pct
                    .saturating_sub(percentage_step)
                    .max(super::LEFT_MACHINES_PCT_MIN);
                true
            }
            KeyCode::Char('j') | KeyCode::Down
                if (self.ui.focus.is_inspector() || self.ui.focus.is_terminal())
                    && self.data.terminal.is_showing() =>
            {
                self.ui.detail_pct = self
                    .ui
                    .detail_pct
                    .saturating_add(percentage_step)
                    .min(super::DETAIL_PCT_MAX);
                true
            }
            KeyCode::Char('k') | KeyCode::Up
                if (self.ui.focus.is_inspector() || self.ui.focus.is_terminal())
                    && self.data.terminal.is_showing() =>
            {
                self.ui.detail_pct = self
                    .ui
                    .detail_pct
                    .saturating_sub(percentage_step)
                    .max(super::DETAIL_PCT_MIN);
                true
            }
            KeyCode::Tab | KeyCode::BackTab => false,
            _ => true,
        }
    }

    async fn handle_dialog_key(&mut self, key: KeyEvent) -> bool {
        if self.ui.active_dialog.is_none() {
            return false;
        }
        let mut dialog = self.ui.active_dialog.take().unwrap();
        let result = dialog.handle_key(key);
        match result {
            EventResult::Message(msg) => {
                if let Some(action) = self
                    .ui
                    .wizard
                    .as_mut()
                    .map(|wizard| wizard.process_message(msg))
                {
                    self.handle_wizard_action(action).await;
                }
                true
            }
            EventResult::Ignored if key.code == KeyCode::Esc => true,
            _ => {
                self.ui.active_dialog = Some(dialog);
                true
            }
        }
    }

    fn handle_modal_mouse(&mut self, mouse: MouseEvent) -> bool {
        // A visible modal always consumes the event even if its component has
        // no mouse behavior.  The priority comes from AppUi::modal_layer(),
        // shared with key dispatch.
        match self.ui.modal_layer() {
            Some(ModalLayer::Dialog) => {
                if let Some(dialog) = &mut self.ui.active_dialog {
                    let _ = dialog.handle_mouse(mouse);
                }
                true
            }
            Some(ModalLayer::Wizard) => {
                if let Some(wizard) = &mut self.ui.wizard {
                    let _ = wizard.handle_mouse(mouse);
                }
                true
            }
            Some(
                ModalLayer::DeleteConfirmation
                | ModalLayer::QuitConfirmation
                | ModalLayer::Help
                | ModalLayer::PowerMenu,
            ) => true,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{contain_wizard_task, quit_confirmation_message};

    #[test]
    fn quit_without_live_resources_needs_no_confirmation() {
        assert_eq!(quit_confirmation_message(0, 0), None);
    }

    #[test]
    fn quit_warning_combines_terminals_and_host_operations() {
        let message = quit_confirmation_message(2, 1).unwrap();

        assert!(message.contains("2 active terminal sessions will be terminated."));
        assert!(message.contains("1 host operation is still running."));
        assert!(message.contains("leave partial host changes"));
        assert!(message.ends_with("Quit anyway?"));
    }

    #[tokio::test]
    async fn wizard_task_panics_are_returned_as_typed_failures() {
        let result: Result<(), crate::application::provisioning::DeploymentError> =
            contain_wizard_task("hardware discovery", async {
                panic!("panic sentinel");
            })
            .await;

        let error = result.unwrap_err();
        assert!(error.to_string().contains("hardware discovery panicked"));
        assert!(error.to_string().contains("panic sentinel"));
    }
}
