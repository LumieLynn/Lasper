//! Terminal session management.

use crate::application::sessions::{
    terminal_session_channel, InteractiveShellEnvironment, SessionSendStatus, SessionService,
    ShellLaunchRequest, ShellTarget, TerminalSessionHandle, TerminalSessionInput,
    ValidatedGuestUserName, WaylandShellRequest,
};
use crate::application::x11::X11AccessService;
use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use crate::domain::session::{SessionSize, TerminalAttachmentKind};
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::term::{BrowserMotion, BrowserSelectionKind};
use crate::tui::foundation::widgets::display::title_tabs::{
    clicked_title_tab, TitleTabHitbox, TitleTabViewport,
};
use crate::tui::protocol::events::AppEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use rio_vt::config::colors::ColorRgb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::mode::{TerminalMode, VisualSelectionMode};
use super::shell_prompt::{run_builtin_shell_prompt, BuiltinShellMode};

/// In-progress mouse text selection.
/// Row is stored as row0-relative: 0 = first drawing row, negative = scrollback.
/// Col is the cell column within the row.
#[derive(Debug, Clone, Default)]
pub struct TextSelection {
    pub active: bool,
    pub anchor: (i32, u16),
    pub extent: (i32, u16),
}

pub struct TerminalSession {
    pub machine_name: String,
    pub guest_user: Option<ValidatedGuestUserName>,
    pub terminal: Arc<parking_lot::Mutex<crate::tui::foundation::term::TerminalCore>>,
    pub input: TerminalSessionInput,
    pub handle: TerminalSessionHandle,
    output_task: tokio::task::JoinHandle<()>,
    pub scroll_offset: usize,
    mode: TerminalMode,
    pub selection: TextSelection,
    /// One-frame flash after yank — cleared after next render.
    pub yanked: bool,
    /// Last resize request successfully queued for the PTY writer.
    queued_size: Option<(u16, u16)>,
    resize_channel_closed: bool,
    scrollback_reclaimed: bool,
    /// Keeps mouse button tracking active while the pointer leaves the pane.
    mouse_capture: bool,
}

#[derive(Debug)]
pub struct SpawnedTerminalSession {
    pub attach_kind: TerminalAttachmentKind,
}

#[derive(Clone, Debug)]
struct RedrawGate {
    pending: Arc<AtomicBool>,
}

impl RedrawGate {
    fn new() -> Self {
        Self {
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }

    fn notify(&self, tx: &tokio::sync::mpsc::Sender<AppEvent>) {
        if self
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && tx.try_send(AppEvent::TerminalRedraw).is_err()
        {
            self.clear();
        }
    }
}

fn spawn_output_parser(
    mut output: tokio::sync::mpsc::Receiver<Vec<u8>>,
    terminal: Arc<parking_lot::Mutex<crate::tui::foundation::term::TerminalCore>>,
    input: TerminalSessionInput,
    app_tx: tokio::sync::mpsc::Sender<AppEvent>,
    redraw_gate: RedrawGate,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(bytes) = output.recv().await {
            let events = {
                let mut parser = terminal.lock();
                let mut events = Vec::new();
                parser.process(&bytes, &mut events);
                events
            };
            for event in events {
                let crate::tui::foundation::term::VtEvent::Reply(reply) = event else {
                    continue;
                };
                if input.send_reply(reply.as_bytes().to_vec()).await == SessionSendStatus::Closed {
                    return;
                }
            }
            redraw_gate.notify(&app_tx);
        }
        redraw_gate.notify(&app_tx);
    })
}

/// Owns terminal sessions and their panel interaction state.
pub struct TerminalManager {
    pub sessions: Vec<TerminalSession>,
    pub active_idx: usize,
    pub show: bool,
    pub maximized: bool,
    /// Inner content area (after borders), set during render for mouse coord conversion.
    pub term_area: Rect,
    /// Last rendered title-tab areas, used before terminal mouse forwarding.
    pub(super) tab_hitboxes: Vec<TitleTabHitbox<usize>>,
    pub(super) tab_viewport: TitleTabViewport,
    session_service: Arc<SessionService>,
    redraw_gate: RedrawGate,
    scrollback_lines: usize,
    query_colors: [ColorRgb; 3],
    host_cursor_style: Option<crate::tui::foundation::term::CursorStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalInputStatus {
    Queued,
    Full,
    Closed,
}

impl TerminalManager {
    pub fn new(session_service: Arc<SessionService>, scrollback_lines: usize) -> Self {
        Self {
            sessions: Vec::new(),
            active_idx: 0,
            show: false,
            maximized: false,
            term_area: Rect::default(),
            tab_hitboxes: Vec::new(),
            tab_viewport: TitleTabViewport::default(),
            session_service,
            redraw_gate: RedrawGate::new(),
            scrollback_lines,
            query_colors: crate::tui::foundation::term::default_query_colors(),
            host_cursor_style: None,
        }
    }

    pub(crate) fn set_host_background_color(&mut self, background: ColorRgb) {
        self.query_colors = crate::tui::foundation::term::query_colors_with_background(background);
        for session in &self.sessions {
            session.terminal.lock().set_query_colors(self.query_colors);
        }
    }

    pub(crate) fn set_host_cursor_style(
        &mut self,
        style: crate::tui::foundation::term::CursorStyle,
    ) {
        self.host_cursor_style = Some(style);
        for session in &self.sessions {
            session.terminal.lock().set_host_cursor_style(style);
        }
    }

    /// Retain the visible terminal screen after a session exits, releasing its
    /// historical rows while keeping the tab and exit status available.
    pub(crate) fn reclaim_exited_scrollback(&mut self) {
        for session in &mut self.sessions {
            if session.scrollback_reclaimed || session.handle.lifecycle().is_running() {
                continue;
            }
            session.terminal.lock().clear_scrollback();
            session.scrollback_reclaimed = true;
            session.scroll_offset = 0;
            session.selection = TextSelection::default();
        }
    }

    // queries

    pub fn is_showing(&self) -> bool {
        self.show
    }

    pub fn active_session(&self) -> Option<&TerminalSession> {
        self.sessions.get(self.active_idx)
    }

    pub(crate) fn in_insert_mode(&self) -> bool {
        self.active_session()
            .is_some_and(TerminalSession::is_insert_mode)
    }

    pub(crate) fn tab_label(&self, index: usize) -> String {
        let Some(session) = self.sessions.get(index) else {
            return String::new();
        };
        let base = session.label();
        let duplicate_count = self
            .sessions
            .iter()
            .filter(|candidate| candidate.label() == base)
            .count();
        if duplicate_count < 2 {
            return base;
        }
        let ordinal = self
            .sessions
            .iter()
            .take(index + 1)
            .filter(|candidate| candidate.label() == base)
            .count();
        format!("{base} #{ordinal}")
    }

    pub fn clear_redraw_pending(&self) {
        self.redraw_gate.clear();
    }

    pub fn wants_mouse_capture(&self) -> bool {
        self.active_session()
            .is_some_and(|session| session.selection.active || session.mouse_capture)
    }

    // lifecycle

    /// Open the terminal attachment selected by the application session service.
    pub async fn spawn(
        &mut self,
        entry: &MachineEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
    ) -> Result<SpawnedTerminalSession, String> {
        self.spawn_with_user(entry, None, WaylandShellRequest::Disabled, rows, app_tx)
            .await
    }

    pub async fn spawn_as_user(
        &mut self,
        entry: &MachineEntry,
        user: ValidatedGuestUserName,
        wayland: WaylandShellRequest,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
    ) -> Result<SpawnedTerminalSession, String> {
        self.spawn_with_user(entry, Some(user), wayland, rows, app_tx)
            .await
    }

    /// Start Lasper's embedded selected-user shell loop.  The first tab state
    /// is a local username prompt; after a valid account is entered the same
    /// session bridges to `OpenMachineShell`/`machinectl shell`.  This keeps
    /// the prompt independent from the host login/getty path and allows every
    /// invocation to create another tab.
    pub async fn spawn_shell_prompt(
        &mut self,
        entry: &MachineEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
    ) -> Result<SpawnedTerminalSession, String> {
        self.spawn_builtin_shell_prompt(entry, rows, app_tx, BuiltinShellMode::Standard)
            .await
    }

    /// Start the same embedded selected-user loop with explicit Host X11
    /// preparation using the same authorization path as `lasper shell`.
    pub async fn spawn_x11_shell_prompt(
        &mut self,
        entry: &MachineEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
        x11_access: Arc<X11AccessService>,
    ) -> Result<SpawnedTerminalSession, String> {
        self.spawn_builtin_shell_prompt(entry, rows, app_tx, BuiltinShellMode::HostX11(x11_access))
            .await
    }

    async fn spawn_builtin_shell_prompt(
        &mut self,
        entry: &MachineEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
        mode: BuiltinShellMode,
    ) -> Result<SpawnedTerminalSession, String> {
        if !entry.access().is_nspawn() {
            return Err(format!(
                "Machine {} is read-only because Lasper did not identify it as an nspawn machine",
                entry.name
            ));
        }
        if entry.state != crate::domain::runtime::MachineState::Running {
            return Err(format!("Machine {} is not running", entry.name));
        }

        let tx = app_tx
            .as_ref()
            .ok_or_else(|| "Internal error: app_tx not set".to_string())?;
        let machine = MachineName::new(&entry.name)
            .map_err(|error| format!("Invalid machine name: {error}"))?;
        let size = SessionSize::new(80, rows)
            .map_err(|error| format!("Invalid terminal size: {error}"))?;
        let id = self.session_service.allocate_id();
        let (mut handle, endpoint) = terminal_session_channel(id, TerminalAttachmentKind::Login);
        let input = handle.input();
        let output = handle
            .take_output()
            .ok_or_else(|| "Internal error: terminal output already attached".to_string())?;
        let terminal = Arc::new(parking_lot::Mutex::new(
            crate::tui::foundation::term::TerminalCore::new_with_host_appearance(
                rows,
                80,
                self.scrollback_lines,
                self.query_colors,
                self.host_cursor_style,
            ),
        ));
        terminal.lock().suppress_initial_line_breaks();
        let service = Arc::clone(&self.session_service);
        tokio::spawn(run_builtin_shell_prompt(
            endpoint, service, machine, size, mode,
        ));
        let output_task = spawn_output_parser(
            output,
            Arc::clone(&terminal),
            input.clone(),
            tx.clone(),
            self.redraw_gate.clone(),
        );
        let session = TerminalSession {
            machine_name: entry.name.clone(),
            guest_user: None,
            terminal,
            input,
            handle,
            output_task,
            scroll_offset: 0,
            mode: TerminalMode::Insert,
            selection: TextSelection::default(),
            yanked: false,
            queued_size: Some((80, rows)),
            resize_channel_closed: false,
            scrollback_reclaimed: false,
            mouse_capture: false,
        };
        self.sessions.push(session);
        self.tab_hitboxes.clear();
        self.active_idx = self.sessions.len() - 1;
        self.show = true;
        Ok(SpawnedTerminalSession {
            attach_kind: TerminalAttachmentKind::Login,
        })
    }

    async fn spawn_with_user(
        &mut self,
        entry: &MachineEntry,
        user: Option<ValidatedGuestUserName>,
        wayland: WaylandShellRequest,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
    ) -> Result<SpawnedTerminalSession, String> {
        if !entry.access().is_nspawn() {
            return Err(format!(
                "Machine {} is read-only because Lasper did not identify it as an nspawn machine",
                entry.name
            ));
        }
        if entry.state != crate::domain::runtime::MachineState::Running {
            return Err(format!("Machine {} is not running", entry.name));
        }

        let tx = app_tx
            .as_ref()
            .ok_or_else(|| "Internal error: app_tx not set".to_string())?;

        let cols = 80;
        let machine = MachineName::new(&entry.name)
            .map_err(|error| format!("Invalid machine name: {error}"))?;
        let size = SessionSize::new(cols, rows)
            .map_err(|error| format!("Invalid terminal size: {error}"))?;
        let mut handle = match user.clone() {
            Some(user) => {
                let request = ShellLaunchRequest::new(
                    ShellTarget::new(machine, user),
                    wayland,
                    InteractiveShellEnvironment::embedded(),
                    size,
                );
                self.session_service
                    .launch_shell(request, None)
                    .await
                    .map(|opened| opened.handle)
                    .map_err(|error| error.to_string())
            }
            None => self
                .session_service
                .open_terminal(machine, size)
                .await
                .map_err(|error| error.to_string()),
        }
        .map_err(|error| format!("Failed to attach terminal: {error}"))?;
        let attach_kind = handle.attachment();
        log::debug!(
            "opened terminal session {} for {}",
            handle.id().get(),
            entry.name
        );
        let input = handle.input();
        let output = handle
            .take_output()
            .ok_or_else(|| "Internal error: terminal output already attached".to_string())?;
        let terminal = Arc::new(parking_lot::Mutex::new(
            crate::tui::foundation::term::TerminalCore::new_with_host_appearance(
                rows,
                cols,
                self.scrollback_lines,
                self.query_colors,
                self.host_cursor_style,
            ),
        ));
        terminal.lock().suppress_initial_line_breaks();
        let output_task = spawn_output_parser(
            output,
            Arc::clone(&terminal),
            input.clone(),
            tx.clone(),
            self.redraw_gate.clone(),
        );

        let session = TerminalSession {
            machine_name: entry.name.clone(),
            guest_user: user,
            terminal,
            input,
            handle,
            output_task,
            scroll_offset: 0,
            mode: TerminalMode::Insert,
            selection: TextSelection::default(),
            yanked: false,
            queued_size: Some((cols, rows)),
            resize_channel_closed: false,
            scrollback_reclaimed: false,
            mouse_capture: false,
        };
        self.sessions.push(session);
        self.tab_hitboxes.clear();
        self.active_idx = self.sessions.len() - 1;
        self.show = true;
        Ok(SpawnedTerminalSession { attach_kind })
    }

    /// Close the active session and report the new active machine, if one
    /// remains. The application uses that name to keep its runtime selection
    /// and detail target in sync with the terminal tab.
    pub fn close_active(&mut self) -> Option<String> {
        if self.sessions.is_empty() {
            self.show = false;
            self.maximized = false;
            self.tab_hitboxes.clear();
            return None;
        }

        let mut session = self.sessions.remove(self.active_idx);
        self.tab_hitboxes.clear();
        session.handle.close();
        session.output_task.abort();

        if self.active_idx >= self.sessions.len() && !self.sessions.is_empty() {
            self.active_idx = self.sessions.len() - 1;
        }

        if self.sessions.is_empty() {
            self.show = false;
            self.maximized = false;
            self.tab_hitboxes.clear();
            None
        } else {
            Some(self.active_machine_name())
        }
    }

    /// Switch to the session matching `entry_name`, or hide the terminal
    /// panel if no session matches.
    pub fn sync_to_entry(&mut self, entry_name: &str) {
        if let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.machine_name == entry_name)
        {
            self.active_idx = idx;
        } else {
            self.show = false;
            self.maximized = false;
        }
    }

    /// Drop all sessions (called on shutdown).
    pub fn cleanup_all(&mut self) {
        for mut session in self.sessions.drain(..) {
            session.handle.close();
            session.output_task.abort();
        }
        self.show = false;
        self.maximized = false;
        self.tab_hitboxes.clear();
        self.redraw_gate.clear();
    }

    // key dispatch
    //
    // A few keys (Tab, q, ?, t) in normal mode intentionally fall through so
    // that workspace shortcuts still work.

    pub fn handle_key(&mut self, key: KeyEvent) -> TerminalKeyOutcome {
        if self.sessions.is_empty() {
            return TerminalKeyOutcome::PassThrough;
        }

        // tab switching (always active)
        let session_count = self.sessions.len();
        let new_idx = match key.code {
            KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => Some(0),
            KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => Some(1),
            KeyCode::Char('3') if key.modifiers.contains(KeyModifiers::ALT) => Some(2),
            KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => Some(3),
            KeyCode::Char('5') if key.modifiers.contains(KeyModifiers::ALT) => Some(4),
            KeyCode::Char('6') if key.modifiers.contains(KeyModifiers::ALT) => Some(5),
            KeyCode::Char('7') if key.modifiers.contains(KeyModifiers::ALT) => Some(6),
            KeyCode::Char('8') if key.modifiers.contains(KeyModifiers::ALT) => Some(7),
            KeyCode::Char('9') if key.modifiers.contains(KeyModifiers::ALT) => Some(8),
            _ => None,
        };

        if let Some(idx) = new_idx {
            if self.select_session(idx) {
                return self.session_changed_outcome();
            }
            return TerminalKeyOutcome::Consumed;
        }

        // per-mode handling
        let idx = self.active_idx;
        let mode = match self.sessions.get(idx) {
            Some(session) => session.interaction_mode(),
            None => return TerminalKeyOutcome::PassThrough,
        };

        if mode.is_insert() {
            let session = match self.sessions.get_mut(idx) {
                Some(s) => s,
                None => return TerminalKeyOutcome::PassThrough,
            };
            // Alt-x toggles out of insert mode
            if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::ALT) {
                session.set_mode(TerminalMode::Normal);
                session.selection = TextSelection::default();
                return TerminalKeyOutcome::Consumed;
            }
            // Forward everything else to the PTY
            let bytes = {
                let terminal = session.terminal.lock();
                let screen = terminal.screen();
                super::encode_key_for_terminal(
                    key,
                    screen.application_cursor(),
                    screen.key_protocol(),
                )
            };
            if !bytes.is_empty() {
                let status = queue_input(&session.input, bytes);
                if matches!(status, TerminalInputStatus::Queued) {
                    session.follow_input_cursor();
                }
                return match status {
                    TerminalInputStatus::Queued => TerminalKeyOutcome::Consumed,
                    TerminalInputStatus::Full => TerminalKeyOutcome::InputQueueFull,
                    TerminalInputStatus::Closed => TerminalKeyOutcome::InputChannelClosed,
                };
            }
            TerminalKeyOutcome::Consumed
        } else {
            // Browser modes — each arm does its own short-lived borrow so we
            // never hold a &mut across a call to self.close_active().
            match key.code {
                KeyCode::Esc if mode.is_visual() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.set_mode(TerminalMode::Normal);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('v')
                    if mode.is_normal() && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.begin_visual_mode(VisualSelectionMode::Block);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('v') if mode.is_normal() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.begin_visual_mode(VisualSelectionMode::Character);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('V') if mode.is_normal() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.begin_visual_mode(VisualSelectionMode::Line);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Enter | KeyCode::Char('i') if mode.is_normal() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.set_mode(TerminalMode::Insert);
                        s.scroll_offset = 0;
                        s.selection = TextSelection::default();
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.set_mode(TerminalMode::Insert);
                        s.scroll_offset = 0;
                        s.selection = TextSelection::default();
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('y') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        let has_native_selection = mode.is_visual()
                            && s.terminal.lock().browser_selection_text().is_some();
                        if has_native_selection || s.selection.anchor != s.selection.extent {
                            copy_selection(s, CopyTarget::Clipboard);
                            if !has_native_selection {
                                s.yanked = true;
                            }
                            if mode.is_visual() {
                                s.set_mode(TerminalMode::Normal);
                            }
                        }
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('x') if mode.is_normal() => match self.close_active() {
                    Some(machine) => TerminalKeyOutcome::SessionChanged(machine),
                    None => TerminalKeyOutcome::Consumed,
                },
                KeyCode::Up | KeyCode::Char('k') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::Up);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Down | KeyCode::Char('j') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::Down);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Left | KeyCode::Char('h') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::Left);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Right | KeyCode::Char('l') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::Right);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Home | KeyCode::Char('0') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::First);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::End | KeyCode::Char('$') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::Last);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('w') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::WordRight);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('b') if mode.is_browser() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.browser_motion(BrowserMotion::WordLeft);
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::PageUp | KeyCode::PageDown
                    if mode.is_browser()
                        && key.modifiers.is_empty()
                        && ScrollAction::from_key(key).is_some() =>
                {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, ScrollAction::from_key(key).expect("page key matched"));
                    }
                    TerminalKeyOutcome::Consumed
                }
                _ if mode.is_normal() && ScrollAction::from_key(key).is_some() => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, ScrollAction::from_key(key).expect("scroll key matched"));
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('1')
                | KeyCode::Char('2')
                | KeyCode::Char('3')
                | KeyCode::Char('4')
                | KeyCode::Char('5')
                | KeyCode::Char('6')
                | KeyCode::Char('7')
                | KeyCode::Char('8')
                | KeyCode::Char('9') => TerminalKeyOutcome::Consumed,

                KeyCode::Char('[') => {
                    let cur = self.active_idx;
                    let idx = if cur == 0 { session_count - 1 } else { cur - 1 };
                    self.select_session(idx);
                    self.session_changed_outcome()
                }
                KeyCode::Char(']') => {
                    let cur = self.active_idx;
                    let idx = (cur + 1) % session_count;
                    self.select_session(idx);
                    self.session_changed_outcome()
                }

                KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('q')
                | KeyCode::Char('?')
                | KeyCode::Char('t')
                | KeyCode::Char('T')
                | KeyCode::Char('R') => TerminalKeyOutcome::PassThrough,

                _ => TerminalKeyOutcome::Consumed,
            }
        }
    }

    // mouse dispatch

    /// Select a rendered terminal tab before the event can reach the PTY.
    pub fn handle_tab_click(&mut self, mouse: MouseEvent) -> Option<String> {
        let idx = clicked_title_tab(&self.tab_hitboxes, mouse)?;
        self.select_session(idx).then(|| self.active_machine_name())
    }

    fn select_session(&mut self, idx: usize) -> bool {
        if idx >= self.sessions.len() {
            return false;
        }
        if let Some(active) = self.sessions.get_mut(self.active_idx) {
            active.mouse_capture = false;
            active.selection.active = false;
        }
        self.active_idx = idx;
        true
    }

    fn active_machine_name(&self) -> String {
        self.sessions[self.active_idx].machine_name.clone()
    }

    fn session_changed_outcome(&self) -> TerminalKeyOutcome {
        TerminalKeyOutcome::SessionChanged(self.active_machine_name())
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> TerminalInputStatus {
        let idx = self.active_idx;
        let session = match self.sessions.get_mut(idx) {
            Some(s) => s,
            None => return TerminalInputStatus::Closed,
        };

        let term_area = self.term_area;
        let inside = mouse.column >= term_area.x
            && mouse.column < term_area.x.saturating_add(term_area.width)
            && mouse.row >= term_area.y
            && mouse.row < term_area.y.saturating_add(term_area.height);
        if !inside && !session.selection.active && !session.mouse_capture {
            return TerminalInputStatus::Queued;
        }

        // Convert absolute screen coords to terminal-relative cell coords.
        let rel_col = mouse.column.saturating_sub(term_area.x);
        let rel_row = mouse.row.saturating_sub(term_area.y);

        let size = {
            let guard = session.terminal.lock();
            let screen = guard.screen();
            screen.size()
        };
        let rel_col = rel_col.min(size.width.saturating_sub(1));
        let rel_row = rel_row.min(size.height.saturating_sub(1));

        if session.is_insert_mode() {
            let (mode, encoding) = {
                let guard = session.terminal.lock();
                (
                    guard.screen().mouse_protocol_mode(),
                    guard.screen().mouse_protocol_encoding(),
                )
            };
            if mode != crate::tui::foundation::term::MouseProtocolMode::None {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    session.mouse_capture = true;
                }
                // Forward to PTY with terminal-relative coords.
                let rel_mouse = MouseEvent {
                    kind: mouse.kind,
                    column: rel_col,
                    row: rel_row,
                    modifiers: mouse.modifiers,
                };
                if let Some(seq) = super::encode_mouse_for_protocol(rel_mouse, mode, encoding) {
                    let status = queue_input(&session.input, seq);
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        session.mouse_capture = false;
                    }
                    return status;
                }
                if matches!(mouse.kind, MouseEventKind::Up(_)) {
                    session.mouse_capture = false;
                }
                return TerminalInputStatus::Queued;
            }
        }

        if session.mode().is_browser() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    session.mouse_capture = true;
                    session.browser_mouse_down(rel_row, rel_col);
                }
                MouseEventKind::Drag(MouseButton::Left) if session.mouse_capture => {
                    session.browser_mouse_drag(rel_row, rel_col);
                }
                MouseEventKind::Up(MouseButton::Left) if session.mouse_capture => {
                    session.mouse_capture = false;
                }
                MouseEventKind::ScrollUp => adjust_scroll(session, ScrollAction::Up(3)),
                MouseEventKind::ScrollDown => adjust_scroll(session, ScrollAction::Down(3)),
                _ => {}
            }
            return TerminalInputStatus::Queued;
        }

        // Insert-mode selection when the application has not enabled a mouse protocol.
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                session.mouse_capture = true;
                let r = rel_row as i32 - session.scroll_offset as i32;
                session.selection = TextSelection {
                    active: true,
                    anchor: (r, rel_col),
                    extent: (r, rel_col),
                };
            }
            MouseEventKind::Drag(MouseButton::Left) if session.selection.active => {
                session.selection.extent = (rel_row as i32 - session.scroll_offset as i32, rel_col);
            }
            MouseEventKind::Up(MouseButton::Left) if session.selection.active => {
                session.selection.active = false;
                session.mouse_capture = false;
                copy_selection(session, CopyTarget::Primary);
            }
            MouseEventKind::ScrollUp => {
                adjust_scroll(session, ScrollAction::Up(3));
            }
            MouseEventKind::ScrollDown => {
                adjust_scroll(session, ScrollAction::Down(3));
            }
            _ => {}
        }
        TerminalInputStatus::Queued
    }
}

impl TerminalSession {
    pub(crate) fn label(&self) -> String {
        match &self.guest_user {
            Some(user) => format!("{}@{}", user, self.machine_name),
            None => self.machine_name.clone(),
        }
    }

    pub fn is_insert_mode(&self) -> bool {
        self.mode.is_insert() && self.handle.lifecycle().is_running()
    }

    pub(crate) fn mode(&self) -> TerminalMode {
        self.interaction_mode()
    }

    fn interaction_mode(&self) -> TerminalMode {
        if self.mode.is_insert() && !self.handle.lifecycle().is_running() {
            TerminalMode::Normal
        } else {
            self.mode
        }
    }

    fn set_mode(&mut self, mode: TerminalMode) {
        let browser = mode.is_browser();
        if mode != self.mode {
            self.selection = TextSelection::default();
        }
        let mut terminal = self.terminal.lock();
        terminal.set_browser_mode(browser);
        if !mode.is_visual() {
            terminal.clear_browser_selection();
        }
        if browser {
            terminal.set_scrollback(self.scroll_offset);
        } else {
            terminal.set_scrollback(0);
        }
        self.scroll_offset = terminal.scrollback_offset();
        drop(terminal);
        self.mode = mode;
    }

    /// Return the embedded terminal viewport to the live input cursor after
    /// input is accepted.  The cursor itself remains owned by rio; this only
    /// changes the local scrollback view.
    fn follow_input_cursor(&mut self) {
        if self.scroll_offset == 0 {
            return;
        }
        self.scroll_offset = 0;
        self.terminal.lock().set_scrollback(0);
    }

    fn begin_visual_mode(&mut self, kind: VisualSelectionMode) {
        self.set_mode(TerminalMode::Visual(kind));
        let selection = match kind {
            VisualSelectionMode::Character => BrowserSelectionKind::Character,
            VisualSelectionMode::Line => BrowserSelectionKind::Line,
            VisualSelectionMode::Block => BrowserSelectionKind::Block,
        };
        self.terminal.lock().begin_browser_selection(selection);
    }

    fn browser_motion(&mut self, motion: BrowserMotion) {
        let mut terminal = self.terminal.lock();
        terminal.browser_motion(motion);
        self.scroll_offset = terminal.scrollback_offset();
    }

    fn browser_goto(&mut self, row: u16, col: u16) {
        let mut terminal = self.terminal.lock();
        terminal.browser_goto(row, col);
        self.scroll_offset = terminal.scrollback_offset();
    }

    fn browser_mouse_down(&mut self, row: u16, col: u16) {
        self.browser_goto(row, col);
        let selection = match self.mode {
            TerminalMode::Visual(VisualSelectionMode::Character) => {
                Some(BrowserSelectionKind::Character)
            }
            TerminalMode::Visual(VisualSelectionMode::Line) => Some(BrowserSelectionKind::Line),
            TerminalMode::Visual(VisualSelectionMode::Block) => Some(BrowserSelectionKind::Block),
            _ => None,
        };
        if let Some(selection) = selection {
            self.terminal.lock().begin_browser_selection(selection);
        }
    }

    fn browser_mouse_drag(&mut self, row: u16, col: u16) {
        if self.mode().is_normal() {
            self.begin_visual_mode(VisualSelectionMode::Character);
        }
        self.browser_goto(row, col);
    }

    pub(crate) fn request_resize(
        &mut self,
        terminal: &mut crate::tui::foundation::term::TerminalCore,
        cols: u16,
        rows: u16,
    ) {
        let current = terminal.screen().size();
        if current.width != cols || current.height != rows {
            terminal.set_size(rows, cols);
            terminal.clear_browser_selection();
            self.selection = TextSelection::default();
        }

        if self.handle.take_resize_failure() {
            self.resize_channel_closed = false;
            self.queued_size = None;
        }
        if self.resize_channel_closed || self.queued_size == Some((cols, rows)) {
            return;
        }

        match queue_resize(&self.input, &mut self.queued_size, cols, rows) {
            QueueResizeResult::Queued => {}
            QueueResizeResult::Full => {}
            QueueResizeResult::Closed => {
                self.resize_channel_closed = true;
                log::debug!("terminal PTY resize channel closed");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueResizeResult {
    Queued,
    Full,
    Closed,
}

fn queue_resize(
    input: &TerminalSessionInput,
    queued_size: &mut Option<(u16, u16)>,
    cols: u16,
    rows: u16,
) -> QueueResizeResult {
    let Ok(size) = SessionSize::new(cols, rows) else {
        return QueueResizeResult::Closed;
    };
    match input.try_resize(size) {
        SessionSendStatus::Queued => {
            *queued_size = Some((cols, rows));
            QueueResizeResult::Queued
        }
        SessionSendStatus::Full => QueueResizeResult::Full,
        SessionSendStatus::Closed => QueueResizeResult::Closed,
    }
}

fn queue_input(input: &TerminalSessionInput, bytes: Vec<u8>) -> TerminalInputStatus {
    match input.try_input(bytes) {
        SessionSendStatus::Queued => TerminalInputStatus::Queued,
        SessionSendStatus::Full => TerminalInputStatus::Full,
        SessionSendStatus::Closed => TerminalInputStatus::Closed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CopyTarget {
    Primary,
    Clipboard,
}

fn copy_selection(session: &TerminalSession, target: CopyTarget) {
    let (ar, ac) = session.selection.anchor;
    let (er, ec) = session.selection.extent;
    let text = {
        let guard = session.terminal.lock();
        match guard.browser_selection_text() {
            Some(text) => text,
            None if ar != er || ac != ec => guard
                .screen()
                .get_selected_text(ac as i32, ar, ec as i32, er),
            None => return,
        }
    };

    if text.is_empty() {
        return;
    }

    let target = match target {
        CopyTarget::Clipboard => super::osc52::SelectionTarget::Clipboard,
        CopyTarget::Primary => super::osc52::SelectionTarget::Primary,
    };
    if let Err(error) = super::osc52::set(target, &text) {
        log::error!("Failed to send OSC 52 clipboard sequence: {error}");
    }
}

/// Adjust a terminal session's scroll offset.
/// Free function so the borrow checker can see it only touches `session`, not `self`.
fn adjust_scroll(session: &mut TerminalSession, action: ScrollAction) {
    let target = {
        let terminal = session.terminal.lock();
        let screen = terminal.screen();
        action.reversed().destination(
            session.scroll_offset,
            screen.row0_count(),
            usize::from(screen.size().height),
        )
    };
    session.scroll_offset = target;
    session.terminal.lock().set_scrollback(target);
}

/// What the terminal key handler wants the caller (App) to do next.
#[derive(Debug, PartialEq)]
pub enum TerminalKeyOutcome {
    /// Key was consumed; stop processing.
    Consumed,
    /// Key was not consumed; continue to workspace shortcuts.
    PassThrough,
    /// The active terminal changed. The application projects its machine onto
    /// the resource selection; the terminal does not mutate runtime data.
    SessionChanged(String),
    /// The PTY writer queue was full; the key was consumed but not delivered.
    InputQueueFull,
    /// The PTY writer is no longer available; the key was consumed.
    InputChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::{
        queue_input, queue_resize, QueueResizeResult, TerminalInputStatus, TerminalKeyOutcome,
        TerminalManager, TerminalSession, TextSelection,
    };
    use crate::adapters::session::{DirectSessionAdapter, DirectTerminalPolicy};
    use crate::application::sessions::{
        journal_session_channel, terminal_session_channel, JournalSessionHandle,
        JournalSessionRequest, SessionError, SessionPort, SessionService, TerminalSessionHandle,
        TerminalSessionRequest, WaylandPreparationRequest, WaylandSessionContext,
        TERMINAL_COMMAND_CAPACITY,
    };
    use crate::domain::machine::MachineName;
    use crate::domain::runtime::{MachineEntry, MachineState};
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use crate::tui::features::workspace::terminal_panel::mode::TerminalMode;
    use crate::tui::features::workspace::terminal_panel::TerminalPanel;
    use crate::tui::foundation::widgets::display::title_tabs::TitleTabHitbox;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RejectingWaylandPort {
        prepare_calls: AtomicUsize,
        open_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SessionPort for RejectingWaylandPort {
        async fn discover_host_wayland_sockets(
            &self,
        ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
            Vec::new()
        }

        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
            Ok(None)
        }

        async fn open_terminal(
            &self,
            request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, crate::application::sessions::TerminalOpenError>
        {
            if let crate::application::sessions::TerminalLaunch::SelectedUserShell {
                user,
                environment,
                ..
            } = &request.launch
            {
                if let Some(host_socket) = environment.wayland_socket() {
                    self.prepare_wayland(WaylandPreparationRequest {
                        probe_id: request.id,
                        target: crate::application::sessions::ShellTarget::new(
                            request.machine.clone(),
                            user.clone(),
                        ),
                        host_socket: host_socket.clone(),
                    })
                    .await
                    .map_err(crate::application::sessions::TerminalOpenError::WaylandPreparation)?;
                }
            }
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn prepare_wayland(
            &self,
            _request: WaylandPreparationRequest,
        ) -> Result<WaylandSessionContext, SessionError> {
            self.prepare_calls.fetch_add(1, Ordering::Relaxed);
            Err(SessionError::new("configured Wayland target is stale"))
        }

        async fn probe_x11_projection(
            &self,
            _request: crate::application::sessions::X11ProjectionProbeRequest,
        ) -> Result<crate::application::sessions::X11ProjectionContext, SessionError> {
            panic!("Wayland terminal tests must not run an X11 probe")
        }

        async fn open_journal(
            &self,
            request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            Ok(journal_session_channel(request.id).0)
        }
    }

    fn host_socket() -> crate::domain::wayland::HostWaylandSocket {
        crate::domain::wayland::HostWaylandSocket::from_verified_parts(
            crate::domain::wayland::WaylandDisplay::new("wayland-0").unwrap(),
            PathBuf::from("/run/user/1000"),
            PathBuf::from("/run/user/1000/wayland-0"),
            1000,
            1000,
            1000,
            0o700,
            crate::domain::wayland::SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    fn terminal_channels() -> (
        crate::application::sessions::TerminalSessionHandle,
        crate::application::sessions::TerminalSessionEndpoint,
    ) {
        terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login)
    }

    fn test_session(
        id: u64,
        machine_name: &str,
    ) -> (
        TerminalSession,
        crate::application::sessions::TerminalSessionEndpoint,
    ) {
        let (handle, endpoint) =
            terminal_session_channel(SessionId::new(id).unwrap(), TerminalAttachmentKind::Login);
        let input = handle.input();
        (
            TerminalSession {
                machine_name: machine_name.into(),
                guest_user: None,
                terminal: Arc::new(parking_lot::Mutex::new(
                    crate::tui::foundation::term::TerminalCore::new_with_host_appearance(
                        24,
                        80,
                        100,
                        crate::tui::foundation::term::default_query_colors(),
                        None,
                    ),
                )),
                input,
                handle,
                output_task: tokio::spawn(async {}),
                scroll_offset: 0,
                mode: TerminalMode::Insert,
                selection: TextSelection::default(),
                yanked: false,
                queued_size: None,
                resize_channel_closed: false,
                scrollback_reclaimed: false,
                mouse_capture: false,
            },
            endpoint,
        )
    }

    #[test]
    fn resize_remains_pending_when_writer_queue_is_full() {
        let (handle, mut endpoint) = terminal_channels();
        let input = handle.input();
        for _ in 0..TERMINAL_COMMAND_CAPACITY {
            assert_eq!(
                input.try_input(vec![b'x']),
                crate::application::sessions::SessionSendStatus::Queued
            );
        }
        let mut queued = Some((80, 24));

        assert_eq!(
            queue_resize(&input, &mut queued, 120, 40),
            QueueResizeResult::Full
        );
        assert_eq!(queued, Some((80, 24)));

        let _ = endpoint.commands.try_recv();
        assert_eq!(
            queue_resize(&input, &mut queued, 120, 40),
            QueueResizeResult::Queued
        );
        assert_eq!(queued, Some((120, 40)));
    }

    #[test]
    fn resize_reports_a_closed_writer_channel() {
        let (handle, endpoint) = terminal_channels();
        let input = handle.input();
        drop(endpoint.commands);
        let mut queued = Some((80, 24));
        assert_eq!(
            queue_resize(&input, &mut queued, 120, 40),
            QueueResizeResult::Closed
        );
        assert_eq!(queued, Some((80, 24)));
    }

    #[test]
    fn input_queue_reports_full_and_closed_channels() {
        let (handle, mut endpoint) = terminal_channels();
        let input = handle.input();
        for _ in 0..TERMINAL_COMMAND_CAPACITY {
            assert_eq!(queue_input(&input, vec![b'a']), TerminalInputStatus::Queued);
        }
        assert_eq!(queue_input(&input, vec![b'b']), TerminalInputStatus::Full);

        let _ = endpoint.commands.try_recv();
        drop(endpoint.commands);
        assert_eq!(queue_input(&input, vec![b'c']), TerminalInputStatus::Closed);
    }

    #[tokio::test]
    async fn spawn_rejects_transitional_machine_states() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        for state in [MachineState::Starting, MachineState::Exiting] {
            let entry = MachineEntry {
                name: "test".into(),
                class: MachineEntry::NSPAWN_CLASS.into(),
                service: MachineEntry::NSPAWN_SERVICE.into(),
                state,
                addresses: Default::default(),
            };
            let error = manager
                .spawn(&entry, 24, &Some(tx.clone()))
                .await
                .expect_err("transitional state must not open a terminal");
            assert!(error.contains("not running"));
        }
    }

    #[tokio::test]
    async fn explicitly_selected_wayland_does_not_fall_back_to_a_plain_shell() {
        let port = Arc::new(RejectingWaylandPort {
            prepare_calls: AtomicUsize::new(0),
            open_calls: AtomicUsize::new(0),
        });
        let service = Arc::new(SessionService::new(port.clone()));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        let entry = MachineEntry {
            name: "demo".into(),
            class: MachineEntry::NSPAWN_CLASS.into(),
            service: MachineEntry::NSPAWN_SERVICE.into(),
            state: MachineState::Running,
            addresses: Default::default(),
        };

        let error = manager
            .spawn_as_user(
                &entry,
                crate::application::sessions::ValidatedGuestUserName::new("alice").unwrap(),
                crate::application::sessions::WaylandShellRequest::SelectedHostDisplay(
                    host_socket(),
                ),
                24,
                &Some(tx),
            )
            .await
            .unwrap_err();

        assert!(error.contains("configured Wayland target is stale"));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn browser_mode_keeps_navigation_local_and_visual_selection_is_native() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, 100);
        let (session, _endpoint) = test_session(1, "demo");
        session
            .terminal
            .lock()
            .process(b"one\r\ntwo\r\nthree", &mut Vec::new());
        manager.sessions.push(session);

        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT,)),
            TerminalKeyOutcome::Consumed
        );
        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            TerminalKeyOutcome::Consumed
        );
        assert!(manager.sessions[0].mode().is_visual());
        assert!(manager.sessions[0]
            .terminal
            .lock()
            .browser_selection_text()
            .is_some());

        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            TerminalKeyOutcome::Consumed
        );
        assert_eq!(manager.sessions.len(), 1);

        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            TerminalKeyOutcome::Consumed
        );
        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TerminalKeyOutcome::Consumed
        );
        assert_eq!(manager.sessions[0].mode(), TerminalMode::Normal);
        assert!(manager.sessions[0].terminal.lock().browser_mode());
    }

    #[tokio::test]
    async fn normal_mouse_drag_enters_visual_with_the_clicked_scrollback_anchor() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, 100);
        let (session, mut endpoint) = test_session(1, "demo");
        session
            .terminal
            .lock()
            .process("abcdef\r\n".repeat(50).as_bytes(), &mut Vec::new());
        manager.sessions.push(session);
        manager.term_area = Rect::new(10, 5, 80, 24);
        let mouse = |kind, column: u16| MouseEvent {
            kind,
            column: 10 + column,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        for (offset, exited) in [(0, false), (5, false), (0, true)] {
            if exited {
                endpoint
                    .lifecycle
                    .send_replace(crate::domain::session::SessionLifecycle::Exited {
                        success: true,
                        code: Some(0),
                    });
            }
            manager.sessions[0].scroll_offset = offset;
            manager.sessions[0].set_mode(TerminalMode::Normal);
            manager.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
            assert_eq!(manager.sessions[0].mode(), TerminalMode::Normal);

            manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
            assert_eq!(manager.sessions[0].mode(), TerminalMode::Normal);
            assert!(manager.wants_mouse_capture());
            assert!(manager.sessions[0]
                .terminal
                .lock()
                .browser_selection_text()
                .is_none());

            manager.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
            manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
            assert_eq!(
                manager.sessions[0].mode(),
                TerminalMode::Visual(super::VisualSelectionMode::Character)
            );
            assert_eq!(manager.sessions[0].scroll_offset, offset);
            assert!(!manager.wants_mouse_capture());
            assert_eq!(
                manager.sessions[0].terminal.lock().browser_selection_text(),
                Some("bcd".into())
            );
            assert!(matches!(
                endpoint.commands.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));

            manager.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(
                manager.sessions[0].terminal.lock().browser_selection_text(),
                Some("bcde".into())
            );
            manager.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(manager.sessions[0].mode(), TerminalMode::Normal);
            manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
            manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1));
            assert_eq!(manager.sessions[0].mode(), TerminalMode::Normal);
        }

        for kind in [
            super::VisualSelectionMode::Line,
            super::VisualSelectionMode::Block,
        ] {
            manager.sessions[0].begin_visual_mode(kind);
            manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
            manager.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
            manager.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
            assert_eq!(manager.sessions[0].mode(), TerminalMode::Visual(kind));
        }
    }

    #[tokio::test]
    async fn insert_mouse_scroll_keeps_cursor_position_and_follows_accepted_input() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, 100);
        let (session, _endpoint) = test_session(1, "demo");
        session
            .terminal
            .lock()
            .process("history\r\n".repeat(50).as_bytes(), &mut Vec::new());
        let cursor_before = session.terminal.lock().screen().cursor_position();
        manager.sessions.push(session);
        manager.term_area = Rect::new(10, 5, 80, 24);

        let status = manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(status, TerminalInputStatus::Queued);
        let offset = manager.sessions[0].scroll_offset;
        assert!(offset > 0);
        assert_eq!(
            manager.sessions[0].terminal.lock().scrollback_offset(),
            offset
        );
        assert_eq!(
            manager.sessions[0]
                .terminal
                .lock()
                .screen()
                .cursor_position(),
            cursor_before
        );

        let panel = TerminalPanel;
        let mut view = Terminal::new(TestBackend::new(80, 26)).unwrap();
        view.draw(|frame| {
            panel.render(frame, frame.area(), &mut manager, true, false);
        })
        .unwrap();
        let status_line = (0..80)
            .map(|column| view.backend().buffer()[(column, 25)].symbol())
            .collect::<String>();
        assert!(status_line.contains("[INSERT]"));
        assert!(status_line.contains("(Scroll:"));

        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            TerminalKeyOutcome::Consumed
        );
        assert_eq!(manager.sessions[0].scroll_offset, 0);
        assert_eq!(manager.sessions[0].terminal.lock().scrollback_offset(), 0);
    }

    #[tokio::test]
    async fn finished_hidden_terminals_release_both_screens_without_touching_live_history() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, 100);
        let (finished, finished_endpoint) = test_session(1, "finished");
        let (running, _running_endpoint) = test_session(2, "running");
        for session in [&finished, &running] {
            session
                .terminal
                .lock()
                .process("history\r\n".repeat(50).as_bytes(), &mut Vec::new());
            assert!(session.terminal.lock().screen().row0_count() > 0);
        }
        // The alternate screen hides the primary screen's accumulated history.
        finished
            .terminal
            .lock()
            .process(b"\x1b[?1049halternate", &mut Vec::new());
        finished_endpoint.lifecycle.send_replace(
            crate::domain::session::SessionLifecycle::Exited {
                success: true,
                code: Some(0),
            },
        );
        manager.sessions = vec![finished, running];
        assert!(!manager.is_showing());
        manager.reclaim_exited_scrollback();

        let mut parser = manager.sessions[0].terminal.lock();
        assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "a");
        parser.process(b"\x1b[?1049l", &mut Vec::new());
        assert_eq!(parser.screen().row0_count(), 0);
        assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "h");
        parser.process("tail\r\n".repeat(50).as_bytes(), &mut Vec::new());
        assert_eq!(parser.screen().row0_count(), 0);
        drop(parser);
        assert!(manager.sessions[1].terminal.lock().screen().row0_count() > 0);

        manager.sessions[0].selection.anchor = (0, 1);
        manager.reclaim_exited_scrollback();
        assert_eq!(manager.sessions[0].selection.anchor, (0, 1));
    }

    #[tokio::test]
    async fn duplicate_terminal_tabs_receive_stable_ordinals() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        let (first, _first_endpoint) = test_session(1, "demo");
        let (second, _second_endpoint) = test_session(2, "demo");
        manager.sessions = vec![first, second];

        assert_eq!(manager.tab_label(0), "demo #1");
        assert_eq!(manager.tab_label(1), "demo #2");
    }

    #[tokio::test]
    async fn rendered_cursor_follows_the_visible_session_and_input_mode() {
        use crate::tui::foundation::term::CursorStyle;

        fn draw_cursor(
            terminal: &mut Terminal<TestBackend>,
            manager: &mut TerminalManager,
            focused: bool,
            resize_mode: bool,
        ) -> Option<CursorStyle> {
            let mut style = None;
            terminal
                .draw(|frame| {
                    style =
                        TerminalPanel.render(frame, frame.area(), manager, focused, resize_mode);
                })
                .unwrap();
            style
        }

        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, 100);
        let (first, _first_endpoint) = test_session(1, "demo");
        let (second, _second_endpoint) = test_session(2, "other");
        manager.sessions = vec![first, second];
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();

        assert_eq!(draw_cursor(&mut terminal, &mut manager, true, false), None);
        assert!(terminal.backend().cursor_visible());
        manager.sessions[0]
            .terminal
            .lock()
            .process(b"\x1b[6 q", &mut Vec::new());
        manager.sessions[1]
            .terminal
            .lock()
            .process(b"\x1b[3 q", &mut Vec::new());

        assert_eq!(
            draw_cursor(&mut terminal, &mut manager, true, false),
            Some(CursorStyle::SteadyBar)
        );
        assert!(terminal.backend().cursor_visible());
        manager.active_idx = 1;
        assert_eq!(
            draw_cursor(&mut terminal, &mut manager, true, false),
            Some(CursorStyle::BlinkingUnderline)
        );
        assert!(terminal.backend().cursor_visible());
        assert_eq!(draw_cursor(&mut terminal, &mut manager, false, false), None);
        assert!(!terminal.backend().cursor_visible());
        assert_eq!(draw_cursor(&mut terminal, &mut manager, true, true), None);

        manager.sessions[1].set_mode(TerminalMode::Normal);
        assert_eq!(
            draw_cursor(&mut terminal, &mut manager, true, false),
            Some(CursorStyle::SteadyBlock)
        );
        manager.sessions[1].set_mode(TerminalMode::Insert);
        manager.sessions[1].scroll_offset = 1;
        assert_eq!(draw_cursor(&mut terminal, &mut manager, true, false), None);
        manager.sessions[1].scroll_offset = 0;
        manager.sessions[1]
            .terminal
            .lock()
            .process(b"\x1b[?25l", &mut Vec::new());
        assert_eq!(
            draw_cursor(&mut terminal, &mut manager, true, false),
            Some(CursorStyle::BlinkingUnderline)
        );
        assert!(!terminal.backend().cursor_visible());
        manager.sessions[1]
            .terminal
            .lock()
            .process(b"\x1b[?25h", &mut Vec::new());
        assert_eq!(
            draw_cursor(&mut terminal, &mut manager, true, false),
            Some(CursorStyle::BlinkingUnderline)
        );
        assert!(terminal.backend().cursor_visible());

        let mut tiny = Terminal::new(TestBackend::new(1, 1)).unwrap();
        assert_eq!(draw_cursor(&mut tiny, &mut manager, true, false), None);
    }

    #[tokio::test]
    async fn terminal_tabs_follow_the_active_session_and_reset_after_widening() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        let mut endpoints = Vec::new();
        for (id, name) in ["alpha", "bravo", "charlie", "delta"]
            .into_iter()
            .enumerate()
        {
            let (session, endpoint) = test_session(id as u64 + 1, name);
            manager.sessions.push(session);
            endpoints.push(endpoint);
        }
        let panel = TerminalPanel;

        let mut narrow = Terminal::new(TestBackend::new(18, 6)).unwrap();
        narrow
            .draw(|frame| {
                panel.render(frame, frame.area(), &mut manager, true, false);
            })
            .unwrap();
        let first_title = (0..18)
            .map(|column| narrow.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert!(!first_title.contains('←'));
        assert!(first_title.contains('→'));
        assert!(manager.tab_hitboxes.iter().any(|tab| tab.value == 0));
        assert!(!manager.tab_hitboxes.iter().any(|tab| tab.value == 3));

        manager.active_idx = 3;
        narrow
            .draw(|frame| {
                panel.render(frame, frame.area(), &mut manager, true, false);
            })
            .unwrap();
        let last_title = (0..18)
            .map(|column| narrow.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert!(last_title.contains('←'));
        assert!(!last_title.contains('→'));
        assert!(last_title.contains("rlie"));
        assert!(!manager.tab_hitboxes.iter().any(|tab| tab.value == 0));
        assert!(manager.tab_hitboxes.iter().any(|tab| tab.value == 3));

        let mut wide = Terminal::new(TestBackend::new(80, 6)).unwrap();
        wide.draw(|frame| {
            panel.render(frame, frame.area(), &mut manager, true, false);
        })
        .unwrap();
        let wide_title = (0..80)
            .map(|column| wide.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert!(!wide_title.contains('←'));
        assert!(!wide_title.contains('→'));
        assert_eq!(
            manager
                .tab_hitboxes
                .iter()
                .map(|tab| tab.value)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );

        drop(endpoints);
    }

    #[tokio::test]
    async fn terminal_tab_click_selects_the_session_and_machine() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        let (first, _first_endpoint) = test_session(1, "first");
        let (second, _second_endpoint) = test_session(2, "second");
        manager.sessions = vec![first, second];
        manager.tab_hitboxes = vec![TitleTabHitbox {
            value: 1,
            area: Rect::new(12, 3, 8, 1),
        }];
        assert_eq!(
            manager.handle_tab_click(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 14,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },),
            Some("second".to_string())
        );
        assert_eq!(manager.active_idx, 1);
    }

    #[tokio::test]
    async fn closing_active_tab_reports_the_new_active_session() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::SystemdTools,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))));
        let mut manager = TerminalManager::new(service, crate::config::DEFAULT_SCROLLBACK_LINES);
        let (mut first, _first_endpoint) = test_session(1, "first");
        let (mut second, _second_endpoint) = test_session(2, "second");
        first.set_mode(TerminalMode::Normal);
        second.set_mode(TerminalMode::Normal);
        manager.sessions = vec![first, second];
        manager.show = true;

        assert_eq!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            TerminalKeyOutcome::SessionChanged("second".into())
        );
        assert_eq!(manager.active_idx, 0);
        assert_eq!(manager.sessions[0].machine_name, "second");
    }
}
