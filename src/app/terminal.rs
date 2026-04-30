//! Terminal session management.

use crate::events::AppEvent;
use crate::nspawn::models::ContainerEntry;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

pub struct TerminalSession {
    pub container_name: String,
    pub terminal:
        Arc<parking_lot::Mutex<vt100::Parser<crate::nspawn::adapters::comm::pty::PtyReply>>>,
    pub pty_tx: tokio::sync::mpsc::Sender<crate::nspawn::adapters::comm::pty::PtyMessage>,
    pub handle: crate::nspawn::adapters::comm::pty::TerminalHandle,
    pub scroll_offset: usize,
    pub insert_mode: bool,
}

/// Holds all terminal-session state that was previously scattered across `AppUi` and `AppData`.
pub struct TerminalManager {
    pub sessions: Vec<TerminalSession>,
    pub active_idx: usize,
    pub show: bool,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active_idx: 0,
            show: false,
        }
    }

    // queries

    pub fn is_showing(&self) -> bool {
        self.show
    }

    pub fn active_session(&self) -> Option<&TerminalSession> {
        self.sessions.get(self.active_idx)
    }

    // lifecycle

    /// Spawn a `machinectl login` PTY for `entry`.  Returns the new session
    /// index on success so the caller can update focus.
    pub fn spawn(
        &mut self,
        entry: &ContainerEntry,
        rows: u16,
        app_tx: &Option<tokio::sync::mpsc::Sender<AppEvent>>,
    ) -> Result<usize, String> {
        if !entry.state.is_running() {
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
            return Ok(idx);
        }

        let tx = app_tx
            .as_ref()
            .ok_or_else(|| "Internal error: app_tx not set".to_string())?;

        let cols: u16 = 80;
        let args: [&str; 2] = ["login", &entry.name];

        crate::nspawn::adapters::comm::pty::spawn_terminal(
            "machinectl",
            &args,
            cols,
            rows,
            tx.clone(),
        )
        .map(|(term, pty_tx, handle)| {
            let session = TerminalSession {
                container_name: entry.name.clone(),
                terminal: term,
                pty_tx,
                handle,
                scroll_offset: 0,
                insert_mode: true,
            };
            self.sessions.push(session);
            self.active_idx = self.sessions.len() - 1;
            self.show = true;
            self.active_idx
        })
        .map_err(|e| format!("Failed to spawn terminal: {}", e))
    }

    pub fn close_active(&mut self) {
        if self.sessions.is_empty() {
            self.show = false;
            return;
        }

        let mut session = self.sessions.remove(self.active_idx);
        session.handle.abort();

        if self.active_idx >= self.sessions.len() && !self.sessions.is_empty() {
            self.active_idx = self.sessions.len() - 1;
        }

        if self.sessions.is_empty() {
            self.show = false;
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
        }
    }

    /// Drop all sessions (called on shutdown).
    pub fn cleanup_all(&mut self) {
        for mut session in self.sessions.drain(..) {
            session.handle.abort();
        }
        self.show = false;
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
            KeyCode::Char('[') => {
                let cur = self.active_idx;
                Some(if cur == 0 { session_count - 1 } else { cur - 1 })
            }
            KeyCode::Char(']') => {
                let cur = self.active_idx;
                Some((cur + 1) % session_count)
            }
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
                return TerminalKeyOutcome::Consumed;
            }
            // Forward everything else to the PTY
            let bytes = crate::ui::views::terminal_panel::encode_key(key);
            let _ = session
                .pty_tx
                .try_send(crate::nspawn::adapters::comm::pty::PtyMessage::Data(bytes));
            TerminalKeyOutcome::Consumed
        } else {
            // Normal mode — each arm does its own short-lived borrow so we
            // never hold a &mut across a call to self.close_active().
            match key.code {
                KeyCode::Enter | KeyCode::Char('i') => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.insert_mode = true;
                        s.scroll_offset = 0;
                    }
                    TerminalKeyOutcome::Consumed
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.insert_mode = true;
                        s.scroll_offset = 0;
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

                KeyCode::Tab | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('t') => {
                    TerminalKeyOutcome::PassThrough
                }

                _ => TerminalKeyOutcome::Consumed,
            }
        }
    }
}

/// Adjust a terminal session's scroll offset.  Free function so the borrow checker can see it only touches `session`, not `self`.
fn adjust_scroll(session: &mut TerminalSession, f: impl FnOnce(usize, usize) -> usize) {
    let mut screen = session.terminal.lock().screen().clone();
    screen.set_scrollback(usize::MAX);
    let max_scroll = screen.scrollback();
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
