use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent};
use futures_util::Stream;
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
    WizardHardwareDiscoveryFinished {
        wizard_id: crate::tui::wizard::WizardInstanceId,
        result: Result<
            crate::application::provisioning::HostHardwareSnapshot,
            crate::application::provisioning::DeploymentError,
        >,
    },
    WizardInterfaceValidationFinished {
        wizard_id: crate::tui::wizard::WizardInstanceId,
        result: Result<
            crate::application::provisioning::InterfaceValidation,
            crate::application::provisioning::DeploymentError,
        >,
    },
    DeploymentPreflightFinished {
        preflight_id: u64,
        request: crate::application::provisioning::DeploymentRequest,
        result: Result<
            crate::application::provisioning::DeploymentPreflight,
            crate::application::provisioning::DeploymentError,
        >,
    },
    DeploymentClaimReleaseFinished {
        wizard_id: crate::tui::wizard::WizardInstanceId,
        deployment_id: crate::application::provisioning::DeploymentId,
        result: Result<(), crate::application::provisioning::DeploymentError>,
    },
    /// Background action execution finished.
    ActionDone(String, crate::tui::StatusLevel),
    /// A machine lifecycle workflow reached a semantic outcome.
    MachineActionFinished(crate::application::MachineLifecycleOutcome),
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
    pub input_done_rx: oneshot::Receiver<Result<(), String>>,
    input_task: Option<tokio::task::JoinHandle<()>>,
    tick_task: Option<tokio::task::JoinHandle<()>>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let (input_done_tx, input_done_rx) = oneshot::channel();
        let (mouse_motion_tx, mouse_motion_rx) = watch::channel(None);

        let tx_key = tx.clone();
        let input_motion_tx = mouse_motion_tx.clone();
        let input_task = tokio::spawn(async move {
            let result = run_input_loop(
                EventStream::new(),
                tx_key,
                input_motion_tx,
                &mut shutdown_rx,
            )
            .await;
            let _ = input_done_tx.send(result);
        });

        // Async tick generator (drift-free)
        let tx_tick = tx.clone();
        let tick_task = tokio::spawn(async move {
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
            input_done_rx,
            input_task: Some(input_task),
            tick_task: Some(tick_task),
        }
    }

    /// Drop the EventStream before the caller restores cooked terminal mode.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.input_task.take() {
            if tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(task) = self.tick_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for EventHandler {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.input_task.take() {
            task.abort();
        }
        if let Some(task) = self.tick_task.take() {
            task.abort();
        }
    }
}

async fn run_input_loop<S>(
    mut reader: S,
    tx: mpsc::Sender<AppEvent>,
    motion_tx: watch::Sender<Option<MouseEvent>>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<(), String>
where
    S: Stream<Item = std::io::Result<CrosstermEvent>> + Unpin,
{
    loop {
        tokio::select! {
            event = reader.next() => {
                match event {
                    Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        tokio::select! {
                            result = tx.send(AppEvent::Key(key)) => {
                                if result.is_err() {
                                    return Ok(());
                                }
                            }
                            _ = &mut *shutdown_rx => return Ok(()),
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        if coalesces_as_pointer_motion(&mouse) {
                            motion_tx.send_replace(Some(mouse));
                        } else {
                            tokio::select! {
                                result = tx.send(AppEvent::Mouse(mouse)) => {
                                    if result.is_err() {
                                        return Ok(());
                                    }
                                }
                                _ = &mut *shutdown_rx => return Ok(()),
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(format!("terminal input read failed: {error}"));
                    }
                    None => return Err("terminal input stream ended unexpectedly".into()),
                }
            }
            _ = &mut *shutdown_rx => return Ok(()),
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

    #[tokio::test]
    async fn input_errors_are_reported_instead_of_silently_stopping() {
        let reader = tokio_stream::iter([Err(std::io::Error::other("input failed"))]);
        let (tx, _rx) = mpsc::channel(1);
        let (motion_tx, _motion_rx) = watch::channel(None);
        let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let error = run_input_loop(reader, tx, motion_tx, &mut shutdown_rx)
            .await
            .unwrap_err();

        assert_eq!(error, "terminal input read failed: input failed");
    }

    #[tokio::test]
    async fn input_eof_is_reported_instead_of_leaving_a_dead_tui() {
        let reader = tokio_stream::empty();
        let (tx, _rx) = mpsc::channel(1);
        let (motion_tx, _motion_rx) = watch::channel(None);
        let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let error = run_input_loop(reader, tx, motion_tx, &mut shutdown_rx)
            .await
            .unwrap_err();

        assert_eq!(error, "terminal input stream ended unexpectedly");
    }
}
