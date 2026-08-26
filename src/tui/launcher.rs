//! Owns the process-level terminal lifecycle for the TUI.
//!
//! The application state and event loop stay in [`super::app`].  This module
//! only acquires the terminal, installs the restoration hook, and hands the
//! initialized terminal to the event loop.  Keeping that boundary here lets a
//! future CLI entry point avoid taking ownership of terminal input entirely.

use anyhow::{Context, Result};
use crossterm::{
    cursor::Show,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::app::App;

struct TerminalRestoreGuard<F: FnMut()> {
    restore: F,
    armed: bool,
    active: Arc<AtomicBool>,
}

impl<F: FnMut()> TerminalRestoreGuard<F> {
    fn new(restore: F, active: Arc<AtomicBool>) -> Self {
        Self {
            restore,
            armed: false,
            active,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
        self.active.store(true, Ordering::Release);
    }

    fn restore(&mut self) {
        if std::mem::take(&mut self.armed) && self.active.swap(false, Ordering::AcqRel) {
            (self.restore)();
        }
    }
}

impl<F: FnMut()> Drop for TerminalRestoreGuard<F> {
    fn drop(&mut self) {
        self.restore();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

fn claim_terminal_restore_for_panic(
    active: &AtomicBool,
    terminal_owner: std::thread::ThreadId,
    panicking_thread: std::thread::ThreadId,
) -> bool {
    panicking_thread == terminal_owner && active.swap(false, Ordering::AcqRel)
}

fn install_panic_hook(terminal_active: Arc<AtomicBool>) {
    let terminal_owner = std::thread::current().id();
    let panic_terminal_active = Arc::clone(&terminal_active);
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let panicking_thread = std::thread::current().id();
        if panicking_thread == terminal_owner {
            if claim_terminal_restore_for_panic(
                &panic_terminal_active,
                terminal_owner,
                panicking_thread,
            ) {
                restore_terminal();
            }
            original_hook(info);
        } else if panic_terminal_active.load(Ordering::Acquire) {
            log::error!("background task panicked while the TUI was active: {info}");
        } else {
            original_hook(info);
        }
    }));
}

/// Run the TUI with exclusive ownership of the process terminal.
pub(crate) async fn run(app: &mut App) -> Result<()> {
    let terminal_active = Arc::new(AtomicBool::new(false));
    install_panic_hook(Arc::clone(&terminal_active));

    let mut terminal_restore =
        TerminalRestoreGuard::new(restore_terminal, Arc::clone(&terminal_active));
    enable_raw_mode().context("Failed to enable raw mode")?;
    terminal_restore.arm();

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize terminal")?;

    let result = app.run(&mut terminal).await;

    log::info!("[lasper] TUI run() completed, restoring terminal...");
    terminal_restore.restore();
    result
}

#[cfg(test)]
mod tests {
    use super::{claim_terminal_restore_for_panic, TerminalRestoreGuard};
    use std::cell::Cell;
    use std::sync::{atomic::AtomicBool, Arc};

    #[test]
    fn armed_guard_restores_on_early_return_and_only_once() {
        let calls = Cell::new(0);
        {
            let active = Arc::new(AtomicBool::new(false));
            let mut guard =
                TerminalRestoreGuard::new(|| calls.set(calls.get() + 1), active.clone());
            guard.arm();
            assert!(active.load(std::sync::atomic::Ordering::Acquire));
        }
        assert_eq!(calls.get(), 1);

        {
            let active = Arc::new(AtomicBool::new(false));
            let mut guard = TerminalRestoreGuard::new(|| calls.set(calls.get() + 1), active);
            guard.arm();
            guard.restore();
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unarmed_guard_does_not_restore() {
        let calls = Cell::new(0);
        {
            let _guard = TerminalRestoreGuard::new(
                || calls.set(calls.get() + 1),
                Arc::new(AtomicBool::new(false)),
            );
        }
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn only_the_terminal_owner_claims_panic_restoration() {
        let owner = std::thread::current().id();
        let background = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        let active = AtomicBool::new(true);

        assert!(!claim_terminal_restore_for_panic(
            &active, owner, background
        ));
        assert!(active.load(std::sync::atomic::Ordering::Acquire));
        assert!(claim_terminal_restore_for_panic(&active, owner, owner));
        assert!(!active.load(std::sync::atomic::Ordering::Acquire));
    }
}
