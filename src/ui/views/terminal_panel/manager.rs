//! Terminal session management.

use crate::events::AppEvent;
use crate::nspawn::models::{ContainerEntry, MachineName};
use crate::nspawn::ops::PermissionLevel;
use crate::nspawn::sys::execution::ExecutionContext;
use crate::nspawn::sys::terminal_attach::TerminalAttachKind;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
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
    pub container_name: String,
    pub attach_kind: TerminalAttachKind,
    pub terminal: Arc<parking_lot::Mutex<crate::term::Parser>>,
    pub pty_tx: tokio::sync::mpsc::Sender<crate::nspawn::adapters::comm::pty::PtyMessage>,
    pub handle: crate::nspawn::adapters::comm::pty::TerminalHandle,
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
    resize_state: crate::nspawn::adapters::comm::pty::ResizeState,
}

#[derive(Debug)]
pub struct SpawnedTerminalSession {
    pub attach_kind: TerminalAttachKind,
}

/// Holds all terminal-session state that was previously scattered across `AppUi` and `AppData`.
pub struct TerminalManager {
    pub sessions: Vec<TerminalSession>,
    pub active_idx: usize,
    pub show: bool,
    pub maximized: bool,
    /// Inner content area (after borders), set during render for mouse coord conversion.
    pub term_area: Rect,
    /// Long-lived clipboard instance so data survives on Linux (ownership model).
    pub clipboard: Option<arboard::Clipboard>,
    redraw_gate: crate::nspawn::adapters::comm::pty::RedrawGate,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active_idx: 0,
            show: false,
            maximized: false,
            term_area: Rect::default(),
            clipboard: None,
            redraw_gate: crate::nspawn::adapters::comm::pty::RedrawGate::new(),
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

    /// Spawn the best supported terminal attachment for `entry`.
    ///
    /// Root/elevated execution can fall back from `machinectl login` to a
    /// closed `nsenter` command when the container has no system bus.
    pub async fn spawn(
        &mut self,
        entry: &ContainerEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
        ctx: &ExecutionContext,
    ) -> Result<SpawnedTerminalSession, String> {
        if entry.state != crate::nspawn::models::ContainerState::Running {
            return Err(format!("Container {} is not running", entry.name));
        }

        // Re-use existing session if one is already open for this container.
        if let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.container_name == entry.name)
        {
            self.active_idx = idx;
            self.show = true;
            return Ok(SpawnedTerminalSession {
                attach_kind: self.sessions[idx].attach_kind,
            });
        }

        let tx = app_tx
            .as_ref()
            .ok_or_else(|| "Internal error: app_tx not set".to_string())?;

        let cols: u16 = 80;

        let (term, pty_tx, handle, resize_state, attach_kind) =
            if let Some(daemon) = ctx.daemon_ref() {
                let spawned = daemon
                    .spawn_terminal(&entry.name, cols, rows)
                    .await
                    .map_err(|e| format!("Failed to attach terminal via daemon: {}", e))?;

                let terminal = crate::nspawn::adapters::comm::pty::spawn_terminal_with_fd(
                    spawned.master_fd,
                    cols,
                    rows,
                    tx.clone(),
                    self.redraw_gate.clone(),
                )
                .map_err(|e| format!("Failed to setup PTY from fd: {}", e))?;
                (
                    terminal.0,
                    terminal.1,
                    terminal.2,
                    terminal.3,
                    spawned.attach_kind,
                )
            } else if ctx.permission_level() == PermissionLevel::Root {
                let name = MachineName::new(&entry.name)
                    .map_err(|error| format!("Invalid machine name: {error}"))?;
                let attach = crate::nspawn::sys::terminal_attach::select(&name)
                    .map_err(|error| format!("Failed to plan terminal attachment: {error}"))?;
                let attach_kind = attach.kind();
                let terminal = crate::nspawn::adapters::comm::pty::spawn_terminal_command(
                    attach.into_pty_command(),
                    cols,
                    rows,
                    tx.clone(),
                    self.redraw_gate.clone(),
                )
                .map_err(|e| format!("Failed to spawn terminal: {}", e))?;
                (terminal.0, terminal.1, terminal.2, terminal.3, attach_kind)
            } else {
                let args: [&str; 3] = ["--", "login", &entry.name];
                let terminal = crate::nspawn::adapters::comm::pty::spawn_terminal(
                    "machinectl",
                    &args,
                    cols,
                    rows,
                    tx.clone(),
                    self.redraw_gate.clone(),
                )
                .map_err(|e| format!("Failed to spawn terminal: {}", e))?;
                (
                    terminal.0,
                    terminal.1,
                    terminal.2,
                    terminal.3,
                    TerminalAttachKind::Login,
                )
            };

        let session = TerminalSession {
            container_name: entry.name.clone(),
            attach_kind,
            terminal: term,
            pty_tx,
            handle,
            scroll_offset: 0,
            insert_mode: true,
            selection: TextSelection::default(),
            yanked: false,
            queued_size: Some((cols, rows)),
            resize_channel_closed: false,
            mouse_capture: false,
            resize_state,
        };
        self.sessions.push(session);
        self.active_idx = self.sessions.len() - 1;
        self.show = true;
        Ok(SpawnedTerminalSession { attach_kind })
    }

    pub fn close_active(&mut self) {
        if self.sessions.is_empty() {
            self.show = false;
            self.maximized = false;
            return;
        }

        let mut session = self.sessions.remove(self.active_idx);
        session.handle.abort();

        if self.active_idx >= self.sessions.len() && !self.sessions.is_empty() {
            self.active_idx = self.sessions.len() - 1;
        }

        if self.sessions.is_empty() {
            self.show = false;
            self.maximized = false;
        }
    }

    /// Switch to the session matching `entry_name`, or hide the terminal
    /// panel if no session matches.
    pub fn sync_to_entry(&mut self, entry_name: &str) {
        if let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.container_name == entry_name)
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
            session.handle.abort();
        }
        self.show = false;
        self.maximized = false;
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
        entries: &[ContainerEntry],
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
            if idx < session_count {
                self.active_idx = idx;
                let name = self.sessions[idx].container_name.clone();
                if let Some(pos) = entries.iter().position(|e| e.name == name) {
                    *selected = pos;
                }
                return TerminalKeyOutcome::ConsumedAndRefreshDetail;
            }
            return TerminalKeyOutcome::Consumed;
        }

        // per-mode handling
        let idx = self.active_idx;
        let insert_mode = match self.sessions.get(idx) {
            Some(s) => s.insert_mode,
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
                let _ = session
                    .pty_tx
                    .try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Data(bytes));
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
                    self.active_idx = if cur == 0 { session_count - 1 } else { cur - 1 };
                    let name = self.sessions[self.active_idx].container_name.clone();
                    if let Some(pos) = entries.iter().position(|e| e.name == name) {
                        *selected = pos;
                    }
                    TerminalKeyOutcome::ConsumedAndRefreshDetail
                }
                KeyCode::Char(']') => {
                    let cur = self.active_idx;
                    self.active_idx = (cur + 1) % session_count;
                    let name = self.sessions[self.active_idx].container_name.clone();
                    if let Some(pos) = entries.iter().position(|e| e.name == name) {
                        *selected = pos;
                    }
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let idx = self.active_idx;
        let session = match self.sessions.get_mut(idx) {
            Some(s) => s,
            None => return,
        };

        let term_area = self.term_area;
        let inside = mouse.column >= term_area.x
            && mouse.column < term_area.x.saturating_add(term_area.width)
            && mouse.row >= term_area.y
            && mouse.row < term_area.y.saturating_add(term_area.height);
        if !inside && !session.selection.active && !session.mouse_capture {
            return;
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

        if session.insert_mode {
            let (mode, encoding) = {
                let guard = session.terminal.lock();
                (
                    guard.screen().mouse_protocol_mode(),
                    guard.screen().mouse_protocol_encoding(),
                )
            };
            if mode != crate::term::screen::MouseProtocolMode::None {
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
                    let _ = session
                        .pty_tx
                        .try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Data(seq));
                }
                if matches!(mouse.kind, MouseEventKind::Up(_)) {
                    session.mouse_capture = false;
                }
                return;
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
    }
}

impl TerminalSession {
    pub(crate) fn request_resize(
        &mut self,
        terminal: &mut crate::term::Parser,
        cols: u16,
        rows: u16,
    ) {
        let current = terminal.screen().size();
        if current.width != cols || current.height != rows {
            terminal.set_size(rows, cols);
            self.selection = TextSelection::default();
        }

        if self.resize_state.take_failure() {
            self.resize_channel_closed = false;
            self.queued_size = None;
        }
        if self.resize_channel_closed || self.queued_size == Some((cols, rows)) {
            return;
        }

        match queue_resize(&self.pty_tx, &mut self.queued_size, cols, rows) {
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
    tx: &tokio::sync::mpsc::Sender<crate::nspawn::adapters::comm::pty::PtyMessage>,
    queued_size: &mut Option<(u16, u16)>,
    cols: u16,
    rows: u16,
) -> QueueResizeResult {
    match tx.try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Resize { cols, rows }) {
        Ok(()) => {
            *queued_size = Some((cols, rows));
            QueueResizeResult::Queued
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => QueueResizeResult::Full,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => QueueResizeResult::Closed,
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
}

#[cfg(test)]
mod tests {
    use super::{queue_resize, QueueResizeResult, TerminalManager};
    use crate::nspawn::adapters::comm::pty::PtyMessage;
    use crate::nspawn::models::{ContainerEntry, ContainerState};

    #[test]
    fn resize_remains_pending_when_writer_queue_is_full() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(PtyMessage::Data(vec![b'x'])).unwrap();
        let mut queued = Some((80, 24));

        assert_eq!(
            queue_resize(&tx, &mut queued, 120, 40),
            QueueResizeResult::Full
        );
        assert_eq!(queued, Some((80, 24)));

        let _ = rx.try_recv();
        assert_eq!(
            queue_resize(&tx, &mut queued, 120, 40),
            QueueResizeResult::Queued
        );
        assert_eq!(queued, Some((120, 40)));
    }

    #[test]
    fn resize_reports_a_closed_writer_channel() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let mut queued = Some((80, 24));
        assert_eq!(
            queue_resize(&tx, &mut queued, 120, 40),
            QueueResizeResult::Closed
        );
        assert_eq!(queued, Some((80, 24)));
    }

    #[tokio::test]
    async fn spawn_rejects_transitional_machine_states() {
        let ctx = crate::nspawn::sys::execution::ExecutionContext::new(
            crate::nspawn::ops::PermissionLevel::User,
            None,
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut manager = TerminalManager::new();
        for state in [ContainerState::Starting, ContainerState::Exiting] {
            let entry = ContainerEntry {
                name: "test".into(),
                state,
                address: None,
                all_addresses: Vec::new(),
            };
            let error = manager
                .spawn(&entry, 24, &Some(tx.clone()), &ctx)
                .await
                .expect_err("transitional state must not open a terminal");
            assert!(error.contains("not running"));
        }
    }
}
