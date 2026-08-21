use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::interval;
use tokio_stream::StreamExt;

fn coalesces_as_pointer_motion(mouse: &MouseEvent) -> bool {
    mouse.kind == crossterm::event::MouseEventKind::Moved
}

/// Events the main loop handles.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    BackendResult(crate::nspawn::ops::BackendResponse),
    DeploymentPreflightFinished {
        preflight_id: u64,
        request: crate::application::provisioning::DeploymentRequest,
        result: Result<
            crate::application::provisioning::DeploymentPreflight,
            crate::application::provisioning::DeploymentError,
        >,
    },
    /// Background action execution finished.
    ActionDone(String, crate::ui::StatusLevel),
    /// A machine lifecycle workflow reached a semantic outcome.
    MachineActionFinished(crate::nspawn::ops::MachineLifecycleOutcome),
    /// Real-time metrics: (container_name, timestamp, cpu_pct, ram_mb)
    MetricsUpdate(String, f64, f64, f64),
    /// Request a UI redraw for the terminal.
    TerminalRedraw,
}

/// Merges keyboard input and periodic ticks into one channel.
pub struct EventHandler {
    pub tx: mpsc::Sender<AppEvent>,
    pub rx: mpsc::Receiver<AppEvent>,
    /// Pointer motion is lossy by design: only the latest position matters,
    /// and it must never queue ahead of keyboard or discrete mouse input.
    pub mouse_motion_rx: watch::Receiver<Option<MouseEvent>>,
    _mouse_motion_tx: watch::Sender<Option<MouseEvent>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let (mouse_motion_tx, mouse_motion_rx) = watch::channel(None);

        // Async keyboard listener — stops immediately on shutdown signal
        // so the EventStream (and its internal stdin thread) is dropped
        // while the terminal is still in raw mode.
        let tx_key = tx.clone();
        let input_motion_tx = mouse_motion_tx.clone();
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
                                if coalesces_as_pointer_motion(&mouse) {
                                    input_motion_tx.send_replace(Some(mouse));
                                } else if tx_key.send(AppEvent::Mouse(mouse)).await.is_err() {
                                    break;
                                }
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
            mouse_motion_rx,
            _mouse_motion_tx: mouse_motion_tx,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    fn mouse(kind: MouseEventKind, column: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn motion_slot_retains_only_the_latest_position() {
        let (tx, mut rx) = watch::channel(None);
        tx.send_replace(Some(mouse(MouseEventKind::Moved, 1)));
        tx.send_replace(Some(mouse(MouseEventKind::Moved, 9)));

        assert_eq!(rx.borrow_and_update().as_ref().unwrap().column, 9);
    }

    #[test]
    fn only_plain_pointer_motion_is_coalesced() {
        assert!(coalesces_as_pointer_motion(&mouse(
            MouseEventKind::Moved,
            0
        )));
        assert!(!coalesces_as_pointer_motion(&mouse(
            MouseEventKind::Drag(MouseButton::Left),
            0,
        )));
        assert!(!coalesces_as_pointer_motion(&mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
        )));
        assert!(!coalesces_as_pointer_motion(&mouse(
            MouseEventKind::ScrollDown,
            0,
        )));
    }
}
