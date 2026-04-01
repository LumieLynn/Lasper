use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};
use std::{
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::Duration,
};
use tokio::sync::mpsc;

/// Events the main loop handles.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    BackendResult(crate::ui::core::BackendResponse),
}

/// Merges keyboard input and periodic ticks into one channel.
/// Call `stop()` when the app quits to release the blocking reader thread.
pub struct EventHandler {
    pub tx: mpsc::Sender<AppEvent>,
    pub rx: mpsc::Receiver<AppEvent>,
    quit:   Arc<AtomicBool>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let quit = Arc::new(AtomicBool::new(false));
        let quit_clone = Arc::clone(&quit);

        // Keyboard reader lives in a blocking thread.
        let tx_key = tx.clone();
        tokio::task::spawn_blocking(move || loop {
            // Check quit flag every iteration (set every ≤50ms by poll timeout).
            if quit_clone.load(Ordering::Relaxed) {
                break;
            }
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if let Ok(CrosstermEvent::Key(key)) = event::read() {
                        if key.kind == KeyEventKind::Press {
                            if tx_key.blocking_send(AppEvent::Key(key)).is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        });

        // Periodic tick
        let tx_tick_clone = tx.clone();
        tokio::spawn(async move {
            let interval = Duration::from_millis(tick_rate_ms);
            loop {
                tokio::time::sleep(interval).await;
                if tx_tick_clone.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        });

        Self { tx, rx, quit }
    }

    /// Signal the blocking keyboard thread to exit within ≤50ms.
    pub fn stop(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}
