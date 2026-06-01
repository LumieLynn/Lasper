use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tokio_stream::StreamExt;

/// Events the main loop handles.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    BackendResult(crate::nspawn::ops::BackendResponse),
    /// Background action execution finished.
    ActionDone(String, crate::ui::StatusLevel),
    /// Real-time metrics: (container_name, timestamp, cpu_pct, ram_mb)
    MetricsUpdate(String, f64, f64, f64),
    /// Request a UI redraw for the terminal.
    TerminalRedraw,
}

/// Merges keyboard input and periodic ticks into one channel.
pub struct EventHandler {
    pub tx: mpsc::Sender<AppEvent>,
    pub rx: mpsc::Receiver<AppEvent>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // Async keyboard listener — stops immediately on shutdown signal
        // so the EventStream (and its internal stdin thread) is dropped
        // while the terminal is still in raw mode.
        let tx_key = tx.clone();
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            loop {
                tokio::select! {
                    event = reader.next() => {
                        match event {
                            Some(Ok(CrosstermEvent::Key(key)))
                                if key.kind == KeyEventKind::Press =>
                            {
                                if tx_key
                                    .send(AppEvent::Key(key))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                                let _ = tx_key.send(AppEvent::Mouse(mouse)).await;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        // Async tick generator (drift-free)
        let tx_tick = tx.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(tick_rate_ms));
            loop {
                ticker.tick().await;
                if tx_tick.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        });

        Self {
            tx,
            rx,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Signal the keyboard-reader task to drop its EventStream immediately.
    /// Must be called while the terminal is still in raw mode so the
    /// internal stdin thread can unblock quickly.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}
