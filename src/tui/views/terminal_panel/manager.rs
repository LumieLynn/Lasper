//! Terminal session management.

use crate::application::sessions::{
    SessionSendStatus, SessionService, TerminalSessionHandle, TerminalSessionInput,
};
use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use crate::domain::session::{SessionSize, TerminalAttachmentKind};
use crate::tui::events::AppEvent;
use crate::tui::views::title_tabs::{clicked_title_tab, TitleTabHitbox};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use arboard::SetExtLinux;

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
    pub attach_kind: TerminalAttachmentKind,
    pub terminal: Arc<parking_lot::Mutex<crate::tui::term::Parser>>,
    pub input: TerminalSessionInput,
    pub handle: TerminalSessionHandle,
    output_task: tokio::task::JoinHandle<()>,
    pub scroll_offset: usize,
    pub insert_mode: bool,
    pub selection: TextSelection,
    /// One-frame flash after yank — cleared after next render.
    pub yanked: bool,
    /// Last resize request successfully queued for the PTY writer.
    queued_size: Option<(u16, u16)>,
    resize_channel_closed: bool,
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
    terminal: Arc<parking_lot::Mutex<crate::tui::term::Parser>>,
    input: TerminalSessionInput,
    app_tx: tokio::sync::mpsc::Sender<AppEvent>,
    redraw_gate: RedrawGate,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(bytes) = output.recv().await {
            let events = {
                let mut parser = terminal.lock();
                let mut events = Vec::new();
                parser.screen.process(&bytes, &mut events);
                events
            };
            for event in events {
                let crate::tui::term::screen::VtEvent::Reply(reply) = event else {
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

/// Holds all terminal-session state that was previously scattered across `AppUi` and `AppData`.
pub struct TerminalManager {
    pub sessions: Vec<TerminalSession>,
    pub active_idx: usize,
    pub show: bool,
    pub maximized: bool,
    /// Inner content area (after borders), set during render for mouse coord conversion.
    pub term_area: Rect,
    /// Last rendered title-tab areas, used before terminal mouse forwarding.
    pub(super) tab_hitboxes: Vec<TitleTabHitbox<usize>>,
    /// Long-lived clipboard instance so data survives on Linux (ownership model).
    pub clipboard: Option<arboard::Clipboard>,
    session_service: Arc<SessionService>,
    redraw_gate: RedrawGate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalInputStatus {
    Queued,
    Full,
    Closed,
}

impl TerminalManager {
    pub fn new(session_service: Arc<SessionService>) -> Self {
        Self {
            sessions: Vec::new(),
            active_idx: 0,
            show: false,
            maximized: false,
            term_area: Rect::default(),
            tab_hitboxes: Vec::new(),
            clipboard: None,
            session_service,
            redraw_gate: RedrawGate::new(),
        }
    }

    // queries

    pub fn is_showing(&self) -> bool {
        self.show
    }

    pub fn active_session(&self) -> Option<&TerminalSession> {
        self.sessions.get(self.active_idx)
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
        if !entry.access().is_nspawn() {
            return Err(format!(
                "Machine {} is read-only because Lasper did not identify it as an nspawn machine",
                entry.name
            ));
        }
        if entry.state != crate::domain::runtime::MachineState::Running {
            return Err(format!("Machine {} is not running", entry.name));
        }

        // Re-use existing session if one is already open for this container.
        if let Some(idx) = self.sessions.iter().position(|session| {
            session.machine_name == entry.name && session.handle.lifecycle().is_running()
        }) {
            self.active_idx = idx;
            self.show = true;
            return Ok(SpawnedTerminalSession {
                attach_kind: self.sessions[idx].attach_kind,
            });
        }

        if let Some(idx) = self
            .sessions
            .iter()
            .position(|session| session.machine_name == entry.name)
        {
            let mut stale = self.sessions.remove(idx);
            self.tab_hitboxes.clear();
            stale.handle.close();
            stale.output_task.abort();
            if self.active_idx > idx {
                self.active_idx -= 1;
            } else if self.active_idx >= self.sessions.len() && !self.sessions.is_empty() {
                self.active_idx = self.sessions.len() - 1;
            }
        }

        let tx = app_tx
            .as_ref()
            .ok_or_else(|| "Internal error: app_tx not set".to_string())?;

        let cols = 80;
        let machine = MachineName::new(&entry.name)
            .map_err(|error| format!("Invalid machine name: {error}"))?;
        let size = SessionSize::new(cols, rows)
            .map_err(|error| format!("Invalid terminal size: {error}"))?;
        let mut handle = self
            .session_service
            .open_terminal(machine, size)
            .await
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
        let terminal = Arc::new(parking_lot::Mutex::new(crate::tui::term::Parser::new(
            rows, cols, 10000,
        )));
        let output_task = spawn_output_parser(
            output,
            Arc::clone(&terminal),
            input.clone(),
            tx.clone(),
            self.redraw_gate.clone(),
        );

        let session = TerminalSession {
            machine_name: entry.name.clone(),
            attach_kind,
            terminal,
            input,
            handle,
            output_task,
            scroll_offset: 0,
            insert_mode: true,
            selection: TextSelection::default(),
            yanked: false,
            queued_size: Some((cols, rows)),
            resize_channel_closed: false,
            mouse_capture: false,
        };
        self.sessions.push(session);
        self.tab_hitboxes.clear();
        self.active_idx = self.sessions.len() - 1;
        self.show = true;
        Ok(SpawnedTerminalSession { attach_kind })
    }

    pub fn close_active(&mut self) {
        if self.sessions.is_empty() {
            self.show = false;
            self.maximized = false;
            self.tab_hitboxes.clear();
            return;
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
    // Returns `true` when the key was consumed by the terminal panel and
    // should not be processed further.  A few keys (Tab, q, ?, t) in normal
    // mode intentionally fall through so that global shortcuts still work.

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        entries: &[MachineEntry],
        selected: &mut usize,
    ) -> TerminalKeyOutcome {
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
            if self.select_session(idx, entries, selected) {
                return TerminalKeyOutcome::ConsumedAndRefreshDetail;
            }
            return TerminalKeyOutcome::Consumed;
        }

        // per-mode handling
        let idx = self.active_idx;
        let insert_mode = match self.sessions.get(idx) {
            Some(session) => session.is_insert_mode(),
            None => return TerminalKeyOutcome::PassThrough,
        };

        if insert_mode {
            let session = match self.sessions.get_mut(idx) {
                Some(s) => s,
                None => return TerminalKeyOutcome::PassThrough,
            };
            // Alt-x toggles out of insert mode
            if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::ALT) {
                session.insert_mode = false;
                session.selection = TextSelection::default();
                return TerminalKeyOutcome::Consumed;
            }
            // Forward everything else to the PTY
            let application_cursor = session.terminal.lock().screen().application_cursor();
            let bytes = super::encode_key_for_mode(key, application_cursor);
            if !bytes.is_empty() {
                return match queue_input(&session.input, bytes) {
                    TerminalInputStatus::Queued => TerminalKeyOutcome::Consumed,
                    TerminalInputStatus::Full => TerminalKeyOutcome::InputQueueFull,
                    TerminalInputStatus::Closed => TerminalKeyOutcome::InputChannelClosed,
                };
            }
            TerminalKeyOutcome::Consumed
        } else {
            // Normal mode — each arm does its own short-lived borrow so we
            // never hold a &mut across a call to self.close_active().
            match key.code {
                KeyCode::Enter | KeyCode::Char('i') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.insert_mode = true;
                        s.scroll_offset = 0;
                        s.selection = TextSelection::default();
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.insert_mode = true;
                        s.scroll_offset = 0;
                        s.selection = TextSelection::default();
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('y') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        if s.selection.anchor != s.selection.extent {
                            copy_selection(s, &mut self.clipboard, CopyTarget::Clipboard);
                            s.yanked = true;
                        }
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('x') => {
                    self.close_active();
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::PageUp => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, |off, max| off.saturating_add(10).min(max));
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::PageDown => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, |off, _max| off.saturating_sub(10));
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, |off, max| off.saturating_add(1).min(max));
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        adjust_scroll(s, |off, _max| off.saturating_sub(1));
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
                    self.select_session(idx, entries, selected);
                    TerminalKeyOutcome::ConsumedAndRefreshDetail
                }
                KeyCode::Char(']') => {
                    let cur = self.active_idx;
                    let idx = (cur + 1) % session_count;
                    self.select_session(idx, entries, selected);
                    TerminalKeyOutcome::ConsumedAndRefreshDetail
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
    pub fn handle_tab_click(
        &mut self,
        mouse: MouseEvent,
        entries: &[MachineEntry],
        selected: &mut usize,
    ) -> bool {
        let Some(idx) = clicked_title_tab(&self.tab_hitboxes, mouse) else {
            return false;
        };
        self.select_session(idx, entries, selected);
        true
    }

    fn select_session(
        &mut self,
        idx: usize,
        entries: &[MachineEntry],
        selected: &mut usize,
    ) -> bool {
        let Some(machine_name) = self
            .sessions
            .get(idx)
            .map(|session| session.machine_name.clone())
        else {
            return false;
        };
        if let Some(active) = self.sessions.get_mut(self.active_idx) {
            active.mouse_capture = false;
            active.selection.active = false;
        }
        self.active_idx = idx;
        if let Some(pos) = entries.iter().position(|entry| entry.name == machine_name) {
            *selected = pos;
        }
        true
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
            if mode != crate::tui::term::screen::MouseProtocolMode::None {
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

        // Selection handling (works in both normal mode and insert mode without mouse protocol).
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
                copy_selection(session, &mut self.clipboard, CopyTarget::Primary);
            }
            MouseEventKind::ScrollUp => {
                adjust_scroll(session, |off, max| off.saturating_add(3).min(max));
            }
            MouseEventKind::ScrollDown => {
                adjust_scroll(session, |off, _max| off.saturating_sub(3));
            }
            _ => {}
        }
        TerminalInputStatus::Queued
    }
}

impl TerminalSession {
    pub fn is_insert_mode(&self) -> bool {
        self.insert_mode && self.handle.lifecycle().is_running()
    }

    pub(crate) fn request_resize(
        &mut self,
        terminal: &mut crate::tui::term::Parser,
        cols: u16,
        rows: u16,
    ) {
        let current = terminal.screen().size();
        if current.width != cols || current.height != rows {
            terminal.set_size(rows, cols);
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

fn copy_selection(
    session: &TerminalSession,
    clipboard: &mut Option<arboard::Clipboard>,
    target: CopyTarget,
) {
    let (ar, ac) = session.selection.anchor;
    let (er, ec) = session.selection.extent;
    if ar == er && ac == ec {
        return; // click without drag — nothing to copy
    }

    // Rows are already row0-relative — no scroll-offset conversion needed.
    let text = {
        let guard = session.terminal.lock();
        guard
            .screen()
            .get_selected_text(ac as i32, ar, ec as i32, er)
    };

    if text.is_empty() {
        return;
    }

    // Lazily initialise clipboard on first copy.
    if clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(c) => *clipboard = Some(c),
            Err(e) => {
                log::error!("Failed to open clipboard: {e}");
                return;
            }
        }
    }

    if let Some(ref mut cb) = clipboard {
        match target {
            CopyTarget::Clipboard => {
                if let Err(e) = cb.set_text(&text) {
                    log::error!("Failed to copy to CLIPBOARD: {e}");
                }
            }
            CopyTarget::Primary => {
                #[cfg(target_os = "linux")]
                if let Err(e) = cb
                    .set()
                    .clipboard(arboard::LinuxClipboardKind::Primary)
                    .text(&text)
                {
                    log::error!("Failed to copy to PRIMARY: {e}");
                }
            }
        }
    }
}

/// Adjust a terminal session's scroll offset.  Free function so the borrow checker can see it only touches `session`, not `self`.
fn adjust_scroll(session: &mut TerminalSession, f: impl FnOnce(usize, usize) -> usize) {
    let max_scroll = session.terminal.lock().screen().row0_count();
    session.scroll_offset = f(session.scroll_offset, max_scroll);
}

/// What the terminal key handler wants the caller (App) to do next.
#[derive(Debug, PartialEq)]
pub enum TerminalKeyOutcome {
    /// Key was consumed; stop processing.
    Consumed,
    /// Key was NOT consumed; continue to overlay / global handlers.
    PassThrough,
    /// Key was consumed AND the detail pane should be refreshed
    /// (tab switch changed the selected container).
    ConsumedAndRefreshDetail,
    /// The PTY writer queue was full; the key was consumed but not delivered.
    InputQueueFull,
    /// The PTY writer is no longer available; the key was consumed.
    InputChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::{
        queue_input, queue_resize, QueueResizeResult, TerminalInputStatus, TerminalManager,
        TerminalSession, TextSelection,
    };
    use crate::adapters::session::{DirectSessionAdapter, DirectTerminalPolicy};
    use crate::application::sessions::{
        terminal_session_channel, SessionService, TERMINAL_COMMAND_CAPACITY,
    };
    use crate::domain::runtime::{MachineEntry, MachineState};
    use crate::domain::session::{SessionId, TerminalAttachmentKind};
    use crate::tui::views::title_tabs::TitleTabHitbox;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use std::sync::Arc;

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
                attach_kind: TerminalAttachmentKind::Login,
                terminal: Arc::new(parking_lot::Mutex::new(crate::tui::term::Parser::new(
                    24, 80, 100,
                ))),
                input,
                handle,
                output_task: tokio::spawn(async {}),
                scroll_offset: 0,
                insert_mode: true,
                selection: TextSelection::default(),
                yanked: false,
                queued_size: None,
                resize_channel_closed: false,
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
        ))));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut manager = TerminalManager::new(service);
        for state in [MachineState::Starting, MachineState::Exiting] {
            let entry = MachineEntry {
                name: "test".into(),
                class: MachineEntry::NSPAWN_CLASS.into(),
                service: MachineEntry::NSPAWN_SERVICE.into(),
                state,
                address: None,
                all_addresses: Vec::new(),
            };
            let error = manager
                .spawn(&entry, 24, &Some(tx.clone()))
                .await
                .expect_err("transitional state must not open a terminal");
            assert!(error.contains("not running"));
        }
    }

    #[tokio::test]
    async fn terminal_tab_click_selects_the_session_and_machine() {
        let service = Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
        ))));
        let mut manager = TerminalManager::new(service);
        let (first, _first_endpoint) = test_session(1, "first");
        let (second, _second_endpoint) = test_session(2, "second");
        manager.sessions = vec![first, second];
        manager.tab_hitboxes = vec![TitleTabHitbox {
            value: 1,
            area: Rect::new(12, 3, 8, 1),
        }];
        let entries = vec![
            MachineEntry {
                name: "first".into(),
                class: MachineEntry::NSPAWN_CLASS.into(),
                service: MachineEntry::NSPAWN_SERVICE.into(),
                state: MachineState::Running,
                address: None,
                all_addresses: Vec::new(),
            },
            MachineEntry {
                name: "second".into(),
                class: MachineEntry::NSPAWN_CLASS.into(),
                service: MachineEntry::NSPAWN_SERVICE.into(),
                state: MachineState::Running,
                address: None,
                all_addresses: Vec::new(),
            },
        ];
        let mut selected = 0;

        assert!(manager.handle_tab_click(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 14,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },
            &entries,
            &mut selected,
        ));
        assert_eq!(manager.active_idx, 1);
        assert_eq!(selected, 1);
    }
}
