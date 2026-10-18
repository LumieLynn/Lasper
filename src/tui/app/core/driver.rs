//! Own the TUI run loop, channel budgets and frame scheduling.
//!
//! Events and completions are forwarded to App; feature state changes happen
//! at their respective workflow boundaries.
use crate::application::RuntimeUpdate;
use crate::tui::app::core::events::EventHandler;
use crate::tui::app::App;
use crate::tui::features::workspace::inspector::refresh::DetailRefreshCompletion;
use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::{Duration, Instant};

const MAX_INPUT_EVENTS_PER_FRAME: usize = 16;
const MAX_BACKGROUND_EVENTS_PER_FRAME: usize = 16;

/// Starts the main application loop.
pub(super) async fn run(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cursor: &mut crate::tui::foundation::term::HostCursor,
) -> Result<()> {
    // Initialize the global theme before any rendering.
    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::load_theme(
        app.config.theme.as_ref(),
    ));

    let mut events = EventHandler::new(100);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<RuntimeUpdate>(4);
    let (detail_refresh_tx, mut detail_refresh_rx) =
        tokio::sync::mpsc::channel::<DetailRefreshCompletion>(1);

    app.ui.app_tx = Some(events.tx.clone());

    // Quit signal — the oneshot fires in the select! below so we
    // break out of the event loop immediately when the user confirms
    // quit, instead of blocking until a background task sends an event.
    let (quit_tx, mut quit_rx) = tokio::sync::oneshot::channel::<()>();
    app.ui.quit_tx = Some(quit_tx);

    // Start nspawn metrics collection engine
    let mut metrics = crate::tui::features::workspace::resources::metrics::spawn_collector(
        events.tx.clone(),
        app.workspace.resources.catalog.cpu_cores,
        app.workspace.resources.catalog.cpu_representation,
    );

    let runtime_observation = app.services.runtime_catalog.observe(refresh_tx);

    let mut mouse_motion_open = true;
    let loop_result = loop {
        // Drain at most 3 refresh batches per frame so rapid background
        // updates can't starve user-input events from the select! below.
        for _ in 0..3 {
            match refresh_rx.try_recv() {
                Ok(update) => app.apply_runtime_update(update),
                Err(_) => break,
            }
        }

        app.prepare_workspace_frame(&detail_refresh_tx);

        // Render a frame
        let render_started = Instant::now();
        let mut cursor_style = None;
        if let Err(error) = terminal.draw(|f| {
            cursor_style = crate::tui::app::draw(f, app);
        }) {
            break Err(anyhow::Error::from(error));
        }
        if let Err(error) = cursor.apply(terminal.backend_mut(), cursor_style) {
            break Err(anyhow::Error::from(error));
        }
        let render_elapsed = render_started.elapsed();
        if render_elapsed >= Duration::from_millis(250) {
            log::warn!("[TUI] render took {} ms", render_elapsed.as_millis());
        }

        tokio::select! {
            biased;
            _ = &mut quit_rx => {
                log::info!("[lasper] select!: quit_rx fired");
                break Ok(());
            }
            input_result = &mut events.input_done_rx => {
                let error = match input_result {
                    Ok(Err(error)) => error,
                    Ok(Ok(())) => "terminal input task stopped unexpectedly".into(),
                    Err(_) => "terminal input task panicked or was cancelled".into(),
                };
                break Err(anyhow::anyhow!(error));
            }
            Some(input) = events.input_rx.recv() => {
                app.handle_input_event(input).await;
                // Foreground input gets its own bounded budget and cannot
                // be delayed behind a burst of backend notifications.
                for _ in 1..MAX_INPUT_EVENTS_PER_FRAME {
                    let Ok(input) = events.input_rx.try_recv() else { break };
                    app.handle_input_event(input).await;
                }
            }
            Some(event) = events.rx.recv() => {
                app.handle_event(event).await;
                // Keep backend work bounded so it cannot starve input or
                // rendering when several observers report at once.
                for _ in 1..MAX_BACKGROUND_EVENTS_PER_FRAME {
                    let Ok(event) = events.rx.try_recv() else { break };
                    app.handle_event(event).await;
                }
            }
            Some(completion) = detail_refresh_rx.recv() => {
                app.apply_detail_refresh(completion);
            }
            motion_changed = events.mouse_motion_rx.changed(), if mouse_motion_open => {
                match motion_changed {
                    Ok(()) => {
                        let mouse = events.mouse_motion_rx.borrow_and_update().as_ref().copied();
                        if let Some(mouse) = mouse {
                            app.handle_mouse(mouse).await;
                        }
                    }
                    Err(_) => mouse_motion_open = false,
                }
            }
            else => {
                log::info!("[lasper] select!: else branch");
                break Ok(());
            }
        }

        if app.should_quit {
            log::info!("[lasper] main loop: should_quit=true, breaking");
            break Ok(());
        }
    };
    log::info!("[lasper] run() cleaning up...");
    runtime_observation.shutdown().await;
    app.shutdown().await;
    metrics.shutdown().await;
    // Shut down the EventStream while the terminal is still in raw mode
    // so the internal stdin thread can unblock quickly instead of
    // blocking on a cooked-mode read after terminal restore.
    events.shutdown().await;
    log::info!("[lasper] run() returning Ok(())");
    loop_result
}
