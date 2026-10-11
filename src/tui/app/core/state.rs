//! Application state, construction and process-wide controls.
//!
//! Feature state belongs to Workspace, terminal sessions and overlays. This
//! module assembles their owners and exposes the application's shared controls.

use crate::composition::ApplicationServices;
use crate::tui::app::core::driver;
use crate::tui::app::core::overlay::{ModalLayer, OverlayState};
use crate::tui::app::core::services::AppServices;
use crate::tui::features::workspace::terminal_panel::TerminalManager;
use crate::tui::features::workspace::Workspace;
use crate::tui::protocol::events::AppEvent;
use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::{Duration, Instant};

pub struct AppUi {
    pub overlays: OverlayState,

    pub status_message: Option<(String, crate::tui::foundation::status::StatusLevel)>,
    pub status_expiry: Option<Instant>,
    pub(crate) footer_hits: crate::tui::foundation::footer::FooterHitboxes,
    pub app_tx: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    /// Signalled when the user confirms quit; included in the main
    /// `select!` so we break immediately instead of waiting for the next
    /// event or channel close.
    pub quit_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AppUi {
    pub fn new() -> Self {
        Self {
            overlays: OverlayState::new(),
            status_message: None,
            status_expiry: None,
            footer_hits: Default::default(),
            app_tx: None,
            quit_tx: None,
        }
    }

    /// Return the interactive topmost overlay. The overlay state also uses
    /// this stacking order to render all underlying layers.
    pub fn modal_layer(&self) -> Option<ModalLayer> {
        self.overlays.modal_layer()
    }

    pub fn leader_active(&self) -> bool {
        self.overlays.leader_active()
    }

    pub fn open_leader(&mut self) {
        self.overlays.open_leader();
    }

    pub fn close_leader(&mut self) {
        self.overlays.close_leader();
    }
}

// App

/// Global application state.
pub struct App {
    pub permissions: std::sync::Arc<dyn crate::composition::PermissionManager>,
    pub config: std::sync::Arc<crate::config::AppConfig>,
    pub should_quit: bool,
    pub(crate) services: AppServices,
    pub(crate) workspace: Workspace,
    pub(crate) terminal: TerminalManager,
    pub ui: AppUi,
}

impl App {
    pub fn new(
        permissions: std::sync::Arc<dyn crate::composition::PermissionManager>,
        systemd_tools: bool,
        log_buffer_lines: usize,
        services: ApplicationServices,
        config: std::sync::Arc<crate::config::AppConfig>,
    ) -> Self {
        let services = AppServices::new(services);
        let scrollback_lines = config.settings.scrollback_lines;
        let terminal = TerminalManager::new(services.session_service.clone(), scrollback_lines);
        Self {
            permissions,
            config,
            should_quit: false,
            services,
            workspace: Workspace::new(systemd_tools, log_buffer_lines),
            terminal,
            ui: AppUi::new(),
        }
    }

    pub fn set_status(&mut self, msg: String, level: crate::tui::foundation::status::StatusLevel) {
        self.set_status_for(msg, level, Duration::from_secs(4));
    }

    pub fn set_status_for(
        &mut self,
        msg: String,
        level: crate::tui::foundation::status::StatusLevel,
        duration: Duration,
    ) {
        self.ui.status_message = Some((msg, level));
        self.ui.status_expiry = Some(Instant::now() + duration);
    }

    /// Fire the quit signal so the event loop breaks out of `select!`
    /// immediately, then mark `should_quit`.
    pub(in crate::tui::app) fn signal_quit(&mut self) {
        log::info!("[lasper] signal_quit() called");
        if let Some(tx) = self.ui.quit_tx.take() {
            let _ = tx.send(());
        }
        self.should_quit = true;
    }

    /// Starts the main application loop.
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        cursor: &mut crate::tui::foundation::term::HostCursor,
    ) -> Result<()> {
        driver::run(self, terminal, cursor).await
    }
}
