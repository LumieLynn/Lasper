//! Shared event timing, status expiry and application shutdown.

use crate::tui::app::core::events::InputEvent;
use crate::tui::app::App;
use crate::tui::protocol::events::AppEvent;
use std::time::{Duration, Instant};

impl App {
    pub(in crate::tui::app) async fn handle_event(&mut self, event: AppEvent) {
        let label = event.label();
        let started = Instant::now();
        self.dispatch_event(event).await;
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(250) {
            log::warn!(
                "[TUI] event handler '{}' took {} ms",
                label,
                elapsed.as_millis()
            );
        }
    }

    pub(in crate::tui::app) async fn shutdown(&mut self) {
        self.shutdown_components().await;
        self.ui.app_tx = None;
        self.ui.quit_tx = None;
    }

    pub(in crate::tui::app) async fn handle_input_event(&mut self, input: InputEvent) {
        if input.is_stale() && !matches!(&input.event, AppEvent::Resize) {
            log::debug!(
                "[TUI] dropping stale foreground {} input (age={} ms)",
                input.event.label(),
                input.age().as_millis()
            );
            return;
        }
        self.handle_event(input.event).await;
    }

    pub(in crate::tui::app) async fn tick(&mut self) {
        // Expire status message
        if let Some(exp) = self.ui.status_expiry {
            if Instant::now() >= exp {
                self.ui.status_message = None;
                self.ui.status_expiry = None;
            }
        }
    }
}
