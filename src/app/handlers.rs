use super::App;
use crate::ui::core::{AppMessage, Component, ContainerMessage, EventResult, ListMessage};
use crate::ui::wizard::StepAction as WizardAction;
use crate::ui::StatusLevel;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

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
        // Modal stack, topmost layer first.  This order mirrors layout.rs.
        if self.handle_dialog_key(key).await {
            return;
        }

        if self.handle_delete_confirm_key(key) {
            return;
        }

        if self.handle_quit_confirm_key(key) {
            return;
        }

        // Layer 3 – overlays (wizard / help / power menu).  Overlays must
        // receive keys before the terminal, even when terminal focus was
        // active when the overlay opened.
        if self.handle_overlay_key(key).await {
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
        if self.ui.focus.active_idx == 3 && self.handle_terminal_focused_key(key).await {
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
            Some(0usize)
        } else if in_rect(col, row, layout.images) {
            Some(1usize)
        } else if !maximized && in_rect(col, row, layout.detail) {
            Some(2usize)
        } else if layout.terminal.is_some_and(|r| in_rect(col, row, r)) {
            Some(3usize)
        } else {
            None
        };

        // Click-to-focus on button press.
        let mut focus_changed = false;
        if let (Some(panel_idx), MouseEventKind::Down(_)) = (hit, mouse.kind) {
            let n = if self.data.terminal.is_showing() {
                4
            } else {
                3
            };
            if panel_idx < n
                && !(self.data.terminal.maximized
                    && self.data.terminal.is_showing()
                    && panel_idx == 2)
            {
                focus_changed = self.ui.focus.active_idx != panel_idx;
                self.set_focus_idx(panel_idx);
            }
        }

        if !maximized && in_rect(col, row, layout.detail) {
            let _ = self.ui.detail_panel.handle_mouse(mouse);
        }

        if focus_changed {
            self.refresh_detail().await;
        }

        // Terminal panel: forward mouse to PTY in insert mode, scroll in normal mode.
        if self.ui.focus.active_idx == 3
            && self.data.terminal.is_showing()
            && (layout.terminal.is_some_and(|r| in_rect(col, row, r))
                || self.data.terminal.wants_mouse_capture())
        {
            match self.data.terminal.handle_mouse(mouse) {
                crate::ui::views::terminal_panel::TerminalInputStatus::Queued => {}
                crate::ui::views::terminal_panel::TerminalInputStatus::Full => self.set_status(
                    "Terminal input queue is full; input was dropped.".into(),
                    StatusLevel::Warn,
                ),
                crate::ui::views::terminal_panel::TerminalInputStatus::Closed => self.set_status(
                    "Terminal input channel is closed.".into(),
                    StatusLevel::Error,
                ),
            }
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
        use crate::ui::views::terminal_panel::TerminalKeyOutcome;

        let outcome =
            self.data
                .terminal
                .handle_key(key, &self.data.entries, &mut self.data.selected);

        // If closing a tab emptied the terminal panel, restore focus to a valid panel.
        if self.ui.focus.active_idx == 3 && !self.data.terminal.is_showing() {
            self.restore_non_terminal_focus();
        }

        match outcome {
            TerminalKeyOutcome::Consumed => true,
            TerminalKeyOutcome::PassThrough => false,
            TerminalKeyOutcome::ConsumedAndRefreshDetail => {
                self.refresh_detail().await;
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
            if let Some(wizard) = &mut self.ui.wizard {
                match wizard.handle_key(key) {
                    WizardAction::None => {}
                    WizardAction::Close => {
                        self.ui.show_wizard = false;
                        self.ui.wizard = None;
                    }
                    WizardAction::Status(msg, level) => {
                        self.set_status(msg, level);
                    }
                    WizardAction::Next | WizardAction::Prev => {}
                    WizardAction::OpenDialog(dialog) => {
                        self.ui.active_dialog = Some(dialog);
                    }
                    WizardAction::CloseDialog => {
                        self.ui.active_dialog = None;
                    }
                }
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
            self.data.exec_ctx.host_operations.active_count(),
        );
        if let Some(message) = message {
            self.ui.quit_dialog = Some(
                crate::ui::widgets::dialogs::confirmation::ConfirmationDialog::new(
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
                self.refresh_detail().await;
                true
            }
            KeyCode::BackTab => {
                self.cycle_main_focus(false);
                self.refresh_detail().await;
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
                        crate::ui::StatusLevel::Info,
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
                        crate::ui::widgets::power_menu::PowerMenu::new_for_images(0)
                    } else {
                        crate::ui::widgets::power_menu::PowerMenu::new(0)
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
                    if self.data.terminal.maximized && self.ui.focus.active_idx == 2 {
                        self.set_focus_idx(3);
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
        match self.ui.focus.active_idx {
            0 => {
                let result = self
                    .ui
                    .container_list
                    .handle_key(key, self.data.entries.len());
                self.handle_container_list_result(result).await;
            }
            1 => {
                let was_internal = self.ui.image_list.shows_internal();
                let image_count = self.active_images().0.len();
                let result = self.ui.image_list.handle_key(key, image_count);
                if was_internal != self.ui.image_list.shows_internal() {
                    self.update_detail_target();
                    self.refresh_detail().await;
                }
                self.handle_container_list_result(result).await;
            }
            2 => {
                let target = self.data.detail_target.clone();
                let result = self.ui.detail_panel.handle_key(key, &target);
                self.handle_detail_panel_result(result).await;
            }
            3 => {
                // Already handled in layer 2; only reached when there are
                // no active sessions (empty terminal panel).
            }
            _ => {}
        }
    }

    async fn handle_container_list_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::List(ListMessage::Next)) => {
                self.select_next();
                if self.image_is_focused() {
                    self.update_detail_target();
                    self.refresh_detail().await;
                } else {
                    self.sync_terminal_to_selected();
                    self.refresh_detail().await;
                }
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.select_prev();
                if self.image_is_focused() {
                    self.update_detail_target();
                    self.refresh_detail().await;
                } else {
                    self.sync_terminal_to_selected();
                    self.refresh_detail().await;
                }
            }
            _ => {}
        }
    }

    async fn handle_detail_panel_result(&mut self, result: EventResult) {
        match result {
            EventResult::Message(AppMessage::Container(ContainerMessage::PaneChanged(_pane))) => {
                self.refresh_detail().await;
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

        let nvidia_installed = crate::nspawn::platform::nvidia::nvidia_ctk_available();
        if let Some(tx) = &self.ui.backend_tx {
            let mut wizard = crate::ui::wizard::Wizard::new(
                self.data.entries.clone(),
                self.data.images.clone(),
                nvidia_installed,
                tx.clone(),
                self.permissions.level(),
                self.data.exec_ctx.clone(),
                self.config.clone(),
            )
            .await;

            if nvidia_installed {
                let _ = tx.try_send(crate::nspawn::ops::BackendCommand::DiscoverHardware);
            } else {
                wizard.context.passthrough.hardware_scanning = false;
            }

            self.ui.wizard = Some(wizard);
            self.ui.show_wizard = true;
        }
    }

    async fn toggle_terminal(&mut self) {
        if self.data.terminal.is_showing() {
            self.data.terminal.show = false;
            if self.ui.focus.active_idx == 3 {
                self.restore_non_terminal_focus();
            }
            self.refresh_detail().await;
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
                    crate::ui::StatusLevel::Warn,
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
                    crate::ui::StatusLevel::Warn,
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
                    self.set_status(error.to_string(), crate::ui::StatusLevel::Error);
                    return;
                }
            };
            let cleanup_supported =
                !image.is_hidden() && crate::nspawn::models::MachineName::new(&image.name).is_ok();
            let mut dialog = crate::ui::widgets::dialogs::confirmation::ConfirmationDialog::new(
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
            crate::ui::StatusLevel::Info,
        );
    }
}

// Resize mode

impl App {
    fn is_terminal_insert_mode(&self) -> bool {
        self.ui.focus.active_idx == 3
            && self
                .data
                .terminal
                .active_session()
                .map(|s| s.insert_mode)
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
            KeyCode::Char('j') | KeyCode::Down if self.ui.focus.active_idx <= 1 => {
                self.ui.left_machines_pct = self
                    .ui
                    .left_machines_pct
                    .saturating_add(percentage_step)
                    .min(super::LEFT_MACHINES_PCT_MAX);
                true
            }
            KeyCode::Char('k') | KeyCode::Up if self.ui.focus.active_idx <= 1 => {
                self.ui.left_machines_pct = self
                    .ui
                    .left_machines_pct
                    .saturating_sub(percentage_step)
                    .max(super::LEFT_MACHINES_PCT_MIN);
                true
            }
            KeyCode::Char('j') | KeyCode::Down
                if self.ui.focus.active_idx >= 2 && self.data.terminal.is_showing() =>
            {
                self.ui.detail_pct = self
                    .ui
                    .detail_pct
                    .saturating_add(percentage_step)
                    .min(super::DETAIL_PCT_MAX);
                true
            }
            KeyCode::Char('k') | KeyCode::Up
                if self.ui.focus.active_idx >= 2 && self.data.terminal.is_showing() =>
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
        match key.code {
            KeyCode::Esc => {
                self.ui.active_dialog = None;
                true
            }
            _ => {
                let mut dialog = self.ui.active_dialog.take().unwrap();
                let result = dialog.handle_key(key);
                match result {
                    EventResult::Message(msg) => {
                        let mut close = false;
                        if let Some(wizard) = &mut self.ui.wizard {
                            let action = wizard.process_message(msg);
                            if matches!(action, crate::ui::wizard::StepAction::CloseDialog) {
                                close = true;
                            }
                        }
                        if close {
                            self.ui.active_dialog = None;
                        }
                        true
                    }
                    _ => {
                        self.ui.active_dialog = Some(dialog);
                        true
                    }
                }
            }
        }
    }

    fn handle_modal_mouse(&mut self, mouse: MouseEvent) -> bool {
        // Keep this order aligned with layout.rs, where the highest layer is
        // rendered last.  A visible modal always consumes the event even if
        // its component has no mouse behavior.
        if let Some(dialog) = &mut self.ui.active_dialog {
            let _ = dialog.handle_mouse(mouse);
            return true;
        }
        if self.ui.delete_dialog.is_some() || self.ui.quit_dialog.is_some() || self.ui.show_help {
            return true;
        }
        if self.ui.show_wizard {
            if let Some(wizard) = &mut self.ui.wizard {
                wizard.handle_mouse(mouse);
            }
            return true;
        }
        self.ui.power_menu.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::quit_confirmation_message;

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
}
