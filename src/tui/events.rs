use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent};
use futures_util::Stream;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_stream::StreamExt;

const POINTER_MOTION_INTERVAL: Duration = Duration::from_millis(33);
const INPUT_CHANNEL_CAPACITY: usize = 128;
const INPUT_MAX_AGE: Duration = Duration::from_millis(500);

fn coalesces_as_pointer_motion(mouse: &MouseEvent) -> bool {
    matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
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

impl AppEvent {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Key(_) => "key",
            Self::Mouse(_) => "mouse",
            Self::Tick => "tick",
            Self::WizardHardwareDiscoveryFinished { .. } => "wizard-hardware-discovery",
            Self::WizardInterfaceValidationFinished { .. } => "wizard-interface-validation",
            Self::DeploymentPreflightFinished { .. } => "deployment-preflight",
            Self::DeploymentClaimReleaseFinished { .. } => "deployment-claim-release",
            Self::ActionDone(_, _) => "action-done",
            Self::MachineActionFinished(_) => "machine-action-finished",
            Self::MetricsUpdate(_, _, _, _) => "metrics-update",
            Self::TerminalRedraw => "terminal-redraw",
        }
    }
}

/// A foreground input event with an expiry time.
///
/// Input is intentionally separate from backend notifications. If an
/// application handler is waiting on a slow host operation, replaying old
/// keystrokes into an embedded PTY is worse than dropping them.
#[derive(Debug)]
pub struct InputEvent {
    pub(crate) event: AppEvent,
    received_at: Instant,
}

impl InputEvent {
    fn new(event: AppEvent) -> Self {
        Self {
            event,
            received_at: Instant::now(),
        }
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.received_at.elapsed() > INPUT_MAX_AGE
    }

    pub(crate) fn age(&self) -> Duration {
        self.received_at.elapsed()
    }

    #[cfg(test)]
    fn with_age(event: AppEvent, age: Duration) -> Self {
        Self {
            event,
            received_at: Instant::now() - age,
        }
    }
}

/// Coordinates foreground input and backend notifications.
pub struct EventHandler {
    pub tx: mpsc::Sender<AppEvent>,
    pub rx: mpsc::Receiver<AppEvent>,
    pub input_rx: mpsc::Receiver<InputEvent>,
    pub mouse_motion_rx: watch::Receiver<Option<MouseEvent>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    pub input_done_rx: oneshot::Receiver<Result<(), String>>,
    input_task: Option<tokio::task::JoinHandle<()>>,
    tick_task: Option<tokio::task::JoinHandle<()>>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let (mouse_motion_tx, mouse_motion_rx) = watch::channel(None);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let (input_done_tx, input_done_rx) = oneshot::channel();

        let tx_input = input_tx;
        let motion_tx = mouse_motion_tx.clone();
        let input_task = tokio::spawn(async move {
            let result =
                run_input_loop(EventStream::new(), tx_input, motion_tx, &mut shutdown_rx).await;
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
            input_rx,
            mouse_motion_rx,
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
    input_tx: mpsc::Sender<InputEvent>,
    motion_tx: watch::Sender<Option<MouseEvent>>,
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
            biased;
            _ = &mut *shutdown_rx => return Ok(()),
            _ = motion_tick.tick(), if pending_motion.is_some() => {
                if let Some(mouse) = pending_motion.take() {
                    motion_tx.send_replace(Some(mouse));
                }
            }
            event = reader.next() => {
                match event {
                    Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        if matches!(
                            try_send_input(&input_tx, AppEvent::Key(key)),
                            InputSendResult::Closed
                        ) {
                            return Ok(());
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        if coalesces_as_pointer_motion(&mouse) {
                            pending_motion = Some(mouse);
                        } else {
                            if let Some(motion) = pending_motion.take() {
                                motion_tx.send_replace(Some(motion));
                            }
                            if matches!(
                                try_send_input(&input_tx, AppEvent::Mouse(mouse)),
                                InputSendResult::Closed
                            ) {
                                return Ok(());
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputSendResult {
    Sent,
    Dropped,
    Closed,
}

fn try_send_input(tx: &mpsc::Sender<InputEvent>, event: AppEvent) -> InputSendResult {
    match tx.try_send(InputEvent::new(event)) {
        Ok(()) => InputSendResult::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => {
            log::debug!("dropping foreground input because the TUI input queue is full");
            InputSendResult::Dropped
        }
        Err(mpsc::error::TrySendError::Closed(_)) => InputSendResult::Closed,
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

    #[tokio::test]
    async fn pointer_motion_is_bounded_and_keeps_the_latest_position() {
        let (source_tx, source_rx) = mpsc::unbounded_channel();
        let reader = tokio_stream::wrappers::UnboundedReceiverStream::new(source_rx);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (motion_tx, mut motion_rx) = watch::channel(None);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let input_task = tokio::spawn(async move {
            run_input_loop(reader, event_tx, motion_tx, &mut shutdown_rx).await
        });

        for column in 1..=9 {
            source_tx
                .send(Ok(CrosstermEvent::Mouse(mouse(
                    MouseEventKind::Moved,
                    column,
                ))))
                .unwrap();
        }

        tokio::time::timeout(Duration::from_millis(150), motion_rx.changed())
            .await
            .expect("coalesced pointer motion was not delivered")
            .expect("motion channel closed");
        let mouse = motion_rx.borrow_and_update().as_ref().copied().unwrap();
        assert_eq!(mouse.column, 9);
        assert!(event_rx.try_recv().is_err());

        let _ = shutdown_tx.send(());
        assert!(input_task.await.unwrap().is_ok());
    }

    #[test]
    fn stale_input_is_rejected_after_a_slow_handler() {
        let input = InputEvent::with_age(
            AppEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE,
            )),
            INPUT_MAX_AGE + Duration::from_millis(1),
        );
        assert!(input.is_stale());
    }

    #[tokio::test]
    async fn full_input_queue_does_not_block_the_reader() {
        let (source_tx, source_rx) = mpsc::unbounded_channel();
        let reader = tokio_stream::wrappers::UnboundedReceiverStream::new(source_rx);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let input_task = tokio::spawn(async move {
            run_input_loop(reader, event_tx, watch::channel(None).0, &mut shutdown_rx).await
        });

        for _ in 0..256 {
            source_tx
                .send(Ok(CrosstermEvent::Key(KeyEvent::new(
                    crossterm::event::KeyCode::Char('x'),
                    crossterm::event::KeyModifiers::NONE,
                ))))
                .unwrap();
        }
        drop(source_tx);

        let result = tokio::time::timeout(Duration::from_millis(150), input_task)
            .await
            .expect("full input queue blocked the reader")
            .unwrap();
        assert_eq!(
            result.unwrap_err(),
            "terminal input stream ended unexpectedly"
        );
        assert!(event_rx.try_recv().is_ok());
    }
}
