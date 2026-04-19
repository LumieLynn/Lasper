use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_stream::StreamExt;

/// Events the main loop handles.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    BackendResult(crate::ui::core::BackendResponse),
    /// Background action execution finished.
    ActionDone(String, crate::ui::StatusLevel),
    /// Real-time metrics: (container_name, timestamp, cpu_pct, ram_mb)
    MetricsUpdate(String, f64, f64, f64),
    /// Real-time log line from a container.
    LogLine(String),
    /// Request a UI redraw for the terminal.
    TerminalRedraw,
}

/// Merges keyboard input and periodic ticks into one channel.
pub struct EventHandler {
    pub tx: mpsc::Sender<AppEvent>,
    pub rx: mpsc::Receiver<AppEvent>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);

        // Async keyboard listener
        let tx_key = tx.clone();
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            while let Some(Ok(event)) = reader.next().await {
                if let CrosstermEvent::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        if tx_key.send(AppEvent::Key(key)).await.is_err() {
                            break;
                        }
                    }
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

        Self { tx, rx }
    }
}
