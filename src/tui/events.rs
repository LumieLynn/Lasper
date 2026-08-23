use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent};
use futures_util::Stream;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, MissedTickBehavior};
use tokio_stream::StreamExt;

const POINTER_MOTION_INTERVAL: Duration = Duration::from_millis(33);

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

        let tx_key = tx.clone();
        let input_task = tokio::spawn(async move {
            let result = run_input_loop(EventStream::new(), tx_key, &mut shutdown_rx).await;
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
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<(), String>
where
    S: Stream<Item = std::io::Result<CrosstermEvent>> + Unpin,
{
    let mut pending_motion = None;
    let mut motion_tick = interval(POINTER_MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    motion_tick.tick().await;

    loop {
        tokio::select! {
            _ = motion_tick.tick(), if pending_motion.is_some() => {
                if !flush_pointer_motion(&tx, &mut pending_motion) {
                    return Ok(());
                }
            }
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
                            pending_motion = Some(mouse);
                        } else {
                            if !flush_pointer_motion(&tx, &mut pending_motion) {
                                return Ok(());
                            }
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

fn flush_pointer_motion(
    tx: &mpsc::Sender<AppEvent>,
    pending_motion: &mut Option<MouseEvent>,
) -> bool {
    let Some(mouse) = pending_motion.take() else {
        return true;
    };
    match tx.try_send(AppEvent::Mouse(mouse)) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
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
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        assert!(pending.replace(mouse(MouseEventKind::Moved, 1)).is_none());
        assert_eq!(
            pending
                .replace(mouse(MouseEventKind::Moved, 9))
                .unwrap()
                .column,
            1
        );

        assert!(flush_pointer_motion(&tx, &mut pending));
        let AppEvent::Mouse(mouse) = rx.try_recv().unwrap() else {
            panic!("expected mouse event");
        };
        assert_eq!(mouse.column, 9);
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
        let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let error = run_input_loop(reader, tx, &mut shutdown_rx)
            .await
            .unwrap_err();

        assert_eq!(error, "terminal input read failed: input failed");
    }

    #[tokio::test]
    async fn input_eof_is_reported_instead_of_leaving_a_dead_tui() {
        let reader = tokio_stream::empty();
        let (tx, _rx) = mpsc::channel(1);
        let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let error = run_input_loop(reader, tx, &mut shutdown_rx)
            .await
            .unwrap_err();

        assert_eq!(error, "terminal input stream ended unexpectedly");
    }

    #[tokio::test]
    async fn pointer_motion_is_bounded_and_keeps_the_latest_position() {
        let (source_tx, source_rx) = mpsc::unbounded_channel();
        let reader = tokio_stream::wrappers::UnboundedReceiverStream::new(source_rx);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let input_task =
            tokio::spawn(async move { run_input_loop(reader, event_tx, &mut shutdown_rx).await });

        for column in 1..=9 {
            source_tx
                .send(Ok(CrosstermEvent::Mouse(mouse(
                    MouseEventKind::Moved,
                    column,
                ))))
                .unwrap();
        }

        let event = tokio::time::timeout(Duration::from_millis(150), event_rx.recv())
            .await
            .expect("coalesced pointer motion was not delivered")
            .expect("application event channel closed");
        let AppEvent::Mouse(mouse) = event else {
            panic!("expected mouse event");
        };
        assert_eq!(mouse.column, 9);
        assert!(event_rx.try_recv().is_err());

        let _ = shutdown_tx.send(());
        assert!(input_task.await.unwrap().is_ok());
    }
}
