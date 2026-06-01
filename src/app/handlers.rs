use super::App;
use crate::ui::core::{AppMessage, Component, ContainerMessage, EventResult, ListMessage};
use crate::ui::wizard::StepAction as WizardAction;
use crate::ui::StatusLevel;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

// Top-level dispatch
//
// handle_key is now a thin chain of mode-specific handlers.  Each handler
// returns `true` when it consumed the key — the remaining handlers are
// skipped.  This replaces the previous 300-line nested match.

impl App {
    pub async fn handle_key(&mut self, key: KeyEvent) {
        // Layer 0 – delete confirmation (modal)
        if self.handle_delete_confirm_key(key) {
            return;
        }

        // Layer 1 – quit confirmation dialog (modal)
        if self.handle_quit_confirm_key(key) {
            return;
        }

        // Layer 1.5 – resize mode (skip when terminal is in insert mode)
        if self.ui.resize_mode == super::ResizeMode::Active
            && !self.is_terminal_insert_mode()
            && self.handle_resize_key(key)
        {
            return;
        }

        // Layer 2.5 – active dialog (modal, blocks everything below)
        if self.handle_dialog_key(key).await {
            return;
        }

        // Layer 2 – terminal panel when it owns focus
        if self.ui.focus.active_idx == 2 && self.handle_terminal_focused_key(key).await {
            return;
        }

        // Layer 3 – overlays (wizard / help / power menu)
        if self.handle_overlay_key(key).await {
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
        // Hit-test: which panel is the mouse over?
        let layout = &self.ui.panel_layout;
        let col = mouse.column;
        let row = mouse.row;

        let maximized = self.data.terminal.is_showing() && self.data.terminal.maximized;

        let hit = if in_rect(col, row, layout.list) {
            Some(0usize)
        } else if !maximized && in_rect(col, row, layout.detail) {
            Some(1usize)
        } else if layout.terminal.is_some_and(|r| in_rect(col, row, r)) {
            Some(2usize)
        } else {
            None
        };

        // Click-to-focus on button press.
        if let (Some(panel_idx), MouseEventKind::Down(_)) = (hit, mouse.kind) {
            let n = if self.data.terminal.is_showing() { 3 } else { 2 };
            if panel_idx < n
                && !(self.data.terminal.maximized
                    && self.data.terminal.is_showing()
                    && panel_idx == 1)
            {
                self.ui.focus.active_idx = panel_idx;
            }
        }

        // Terminal panel: forward mouse to PTY in insert mode, scroll in normal mode.
        if self.ui.focus.active_idx == 2 && self.data.terminal.is_showing() {
            self.data.terminal.handle_mouse(mouse);
        }
    }
}

fn in_rect(col: u16, row: u16, r: ratatui::layout::Rect) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
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
            _ => {}
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
        if self.ui.focus.active_idx == 2 && !self.data.terminal.is_showing() {
            self.ui.focus.active_idx = if self.ui.prev_active_idx == 2 {
                0
            } else {
                self.ui.prev_active_idx
            };
        }

        match outcome {
            TerminalKeyOutcome::Consumed => true,
            TerminalKeyOutcome::PassThrough => false,
            TerminalKeyOutcome::ConsumedAndRefreshDetail => {
                self.refresh_detail().await;
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
                    self.ui.power_menu = None;
                    match idx {
                        0 => self.action_start(),
                        1 => self.action_poweroff(),
                        2 => self.action_reboot(),
                        3 => self.action_terminate(),
                        4 => self.action_kill(),
                        5 => self.action_enable(),
                        6 => self.action_disable(),
                        7 => self.show_delete_dialog(),
                        _ => {}
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
    async fn handle_global_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.data.terminal.sessions.is_empty() {
                    self.ui.quit_dialog =
                        Some(crate::ui::widgets::dialogs::confirmation::ConfirmationDialog::new(
                            "Quit Lasper?",
                            "Active terminal sessions are still running.\nQuit and terminate all logins?",
                        ));
                } else {
                    self.signal_quit();
                }
                true
            }
            KeyCode::Char('?') => {
                self.ui.show_help = true;
                true
            }
            KeyCode::Tab => {
                let n = if self.data.terminal.is_showing() {
                    3
                } else {
                    2
                };
                self.ui.focus.cycle_forward(n);
                if self.data.terminal.is_showing()
                    && self.data.terminal.maximized
                    && self.ui.focus.active_idx == 1
                {
                    self.ui.focus.cycle_forward(n);
                }
                true
            }
            KeyCode::BackTab => {
                let n = if self.data.terminal.is_showing() {
                    3
                } else {
                    2
                };
                self.ui.focus.cycle_backward(n);
                if self.data.terminal.is_showing()
                    && self.data.terminal.maximized
                    && self.ui.focus.active_idx == 1
                {
                    self.ui.focus.cycle_backward(n);
                }
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
                if !self.data.entries.is_empty() {
                    self.ui.power_menu = Some(crate::ui::widgets::power_menu::PowerMenu::new(0));
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
                    if self.data.terminal.maximized && self.ui.focus.active_idx == 1 {
                        self.ui.focus.active_idx = 2;
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
                let result = self.ui.container_list.handle_key(key);
                self.handle_container_list_result(result).await;
            }
            1 => {
                let result = self.ui.detail_panel.handle_key(key);
                self.handle_detail_panel_result(result).await;
            }
            2 => {
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
                self.sync_terminal_to_selected();
                self.refresh_detail().await;
            }
            EventResult::Message(AppMessage::List(ListMessage::Prev)) => {
                self.select_prev();
                self.sync_terminal_to_selected();
                self.refresh_detail().await;
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
            self.set_status(
                "Root required — run: sudo lasper".into(),
                StatusLevel::Error,
            );
            return;
        }

        let nvidia_installed = crate::nspawn::platform::nvidia::nvidia_ctk_available();
        if let Some(tx) = &self.ui.backend_tx {
            let mut wizard = crate::ui::wizard::Wizard::new(
                self.data.entries.clone(),
                nvidia_installed,
                tx.clone(),
                self.permissions.level(),
                self.data.exec_ctx.clone(),
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
            if self.ui.focus.active_idx == 2 {
                self.ui.focus.active_idx = if self.ui.prev_active_idx == 2 {
                    0
                } else {
                    self.ui.prev_active_idx
                };
            }
        } else {
            self.spawn_terminal().await;
        }
    }

    fn show_delete_dialog(&mut self) {
        let entry = match self.data.entries.get(self.data.selected) {
            Some(e) => e,
            None => return,
        };
        if entry.state.is_running() {
            self.set_status(
                format!("Stop '{}' before deleting it.", entry.name),
                crate::ui::StatusLevel::Warn,
            );
            return;
        }
        self.ui.delete_dialog = Some(
            crate::ui::widgets::dialogs::confirmation::ConfirmationDialog::new(
                "Delete Container",
                format!(
                    "Delete '{}' and all its data?\nThis cannot be undone.",
                    entry.name
                ),
            ),
        );
    }
}

// Resize mode

impl App {
    fn is_terminal_insert_mode(&self) -> bool {
        self.ui.focus.active_idx == 2
            && self
                .data
                .terminal
                .active_session()
                .map(|s| s.insert_mode)
                .unwrap_or(false)
    }

    fn handle_resize_key(&mut self, key: KeyEvent) -> bool {
        let step = 5u16;

        match key.code {
            KeyCode::Esc | KeyCode::Char('R') | KeyCode::Char('q') => {
                self.ui.resize_mode = super::ResizeMode::Inactive;
                true
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.ui.container_list_pct = self
                    .ui
                    .container_list_pct
                    .saturating_sub(step)
                    .max(super::CONTAINER_LIST_PCT_MIN);
                true
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.ui.container_list_pct = self
                    .ui
                    .container_list_pct
                    .saturating_add(step)
                    .min(super::CONTAINER_LIST_PCT_MAX);
                true
            }
            KeyCode::Char('j') | KeyCode::Down if self.data.terminal.is_showing() => {
                self.ui.detail_pct = self
                    .ui
                    .detail_pct
                    .saturating_add(step)
                    .min(super::DETAIL_PCT_MAX);
                true
            }
            KeyCode::Char('k') | KeyCode::Up if self.data.terminal.is_showing() => {
                self.ui.detail_pct = self
                    .ui
                    .detail_pct
                    .saturating_sub(step)
                    .max(super::DETAIL_PCT_MIN);
                true
            }
            KeyCode::Tab => false,
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
}
