//! Inspector state, background reads, journal sessions and content panes.

pub mod core;
pub mod log_manager;
pub(crate) mod messages;
pub mod panes;
mod presentation;
pub(crate) mod refresh;
pub(crate) mod state;

pub use core::manager::DetailPanel;
pub use core::model::{DetailPane, DetailTarget};
pub(crate) use core::view_data::{DetailLogData, DetailRenderContext};

use crate::domain::runtime::MachineEntry;
use crate::tui::features::workspace::inspector::log_manager::LogManager;
use crate::tui::features::workspace::inspector::refresh::{
    DetailRefreshCompletion, DetailRefreshServices, PreparedDetailRefresh,
};
use crate::tui::features::workspace::inspector::state::{DetailRefreshEffects, DetailState};
use crate::tui::features::workspace::resources::catalog::ResourceCatalog;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};
use tokio::sync::mpsc::Sender;

pub(crate) struct Inspector {
    pub(crate) state: DetailState,
    pub(crate) panel: DetailPanel,
    pub(crate) logs: LogManager,
}

impl Inspector {
    pub(crate) fn new(log_buffer_lines: usize) -> Self {
        Self {
            state: DetailState::new(),
            panel: DetailPanel::new(),
            logs: LogManager::new(log_buffer_lines),
        }
    }

    pub(crate) fn target(&self) -> &DetailTarget {
        &self.state.target
    }

    pub(crate) fn invalidate_overview(&mut self) {
        self.state.overview_dirty = true;
    }

    pub(crate) fn retain_machine_logs(&mut self, machines: &[MachineEntry]) {
        let active_names = machines
            .iter()
            .map(|machine| machine.name.clone())
            .collect();
        self.logs.remove_stale(&active_names);
    }

    pub(crate) fn drain_logs(&mut self) {
        self.logs.drain_all();
    }

    pub(crate) fn set_target(&mut self, target: DetailTarget) {
        if self.state.replace_target(target) {
            self.panel.ensure_pane_for_target(&self.state.target);
        }
    }

    pub(crate) fn active_pane(&self) -> DetailPane {
        self.panel.active_pane()
    }

    pub(crate) fn request_refresh(&mut self) {
        self.state
            .refresh
            .request(self.state.target.clone(), self.active_pane());
    }

    pub(crate) fn start_refresh(
        &mut self,
        machines: &[MachineEntry],
        services: DetailRefreshServices,
        tx: &Sender<DetailRefreshCompletion>,
    ) -> DetailRefreshEffects {
        let Some(prepared) =
            self.state
                .prepare_refresh(self.active_pane(), machines, &mut self.logs)
        else {
            return DetailRefreshEffects::default();
        };
        match prepared {
            PreparedDetailRefresh::Ready(completion) => self.apply_refresh(completion),
            PreparedDetailRefresh::Job(job) => {
                let tx = tx.clone();
                self.state.refresh.track_task(tokio::spawn(async move {
                    let completion = job.execute(services).await;
                    let _ = tx.send(completion).await;
                }));
                DetailRefreshEffects::default()
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn refresh_now(
        &mut self,
        machines: &[MachineEntry],
        services: DetailRefreshServices,
    ) -> DetailRefreshEffects {
        self.request_refresh();
        let Some(prepared) =
            self.state
                .prepare_refresh(self.active_pane(), machines, &mut self.logs)
        else {
            return DetailRefreshEffects::default();
        };
        let completion = match prepared {
            PreparedDetailRefresh::Ready(completion) => completion,
            PreparedDetailRefresh::Job(job) => job.execute(services).await,
        };
        self.apply_refresh(completion)
    }

    pub(crate) fn apply_refresh(
        &mut self,
        completion: DetailRefreshCompletion,
    ) -> DetailRefreshEffects {
        let effects = self
            .state
            .apply_refresh(completion, self.active_pane(), &mut self.logs);
        if effects.reset_config_scroll {
            self.panel.config_scroll.reset();
        }
        effects
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> crate::tui::protocol::EventResult {
        self.panel.handle_key(key, &self.state.target)
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> crate::tui::protocol::EventResult {
        self.panel.handle_mouse(mouse, &self.state.target)
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        catalog: &ResourceCatalog,
        focused: bool,
        resize_mode: bool,
    ) {
        self.panel.set_focus(focused);
        let mut data = self.state.render_context(catalog, &mut self.logs);
        self.panel.render(frame, area, &mut data, resize_mode);
    }

    pub(crate) fn register_footer_hints(
        &self,
        footer: &mut crate::tui::foundation::footer::FooterRegistry,
    ) {
        self.panel.register_footer_hints(footer, &self.state.target);
    }

    pub(crate) async fn shutdown(&mut self) {
        self.state.refresh.shutdown().await;
        self.logs.cleanup_all();
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::features::workspace::inspector::refresh::{
        DetailRefreshCompletion, DetailRefreshResult,
    };
    use crate::tui::features::workspace::inspector::{DetailTarget, Inspector};

    #[test]
    fn changing_targets_rejects_the_old_read_without_consuming_the_new_slot() {
        let mut inspector = Inspector::new(20);
        inspector.set_target(DetailTarget::Machine("old".into()));
        inspector.request_refresh();
        let old = inspector.state.refresh.take_pending().unwrap();

        inspector.set_target(DetailTarget::Machine("current".into()));
        inspector.logs.get_or_create("current");
        inspector.logs.push_line("current", "current log");
        inspector.request_refresh();
        let current = inspector.state.refresh.take_pending().unwrap();
        assert_ne!(current.revision, old.revision);

        let effects = inspector.apply_refresh(DetailRefreshCompletion {
            ticket: old,
            result: DetailRefreshResult::Empty,
        });
        assert!(effects.status.is_none());
        assert!(!effects.reset_config_scroll);
        assert_eq!(inspector.target(), &DetailTarget::Machine("current".into()));
        assert_eq!(inspector.logs.buffer_mut("current").unwrap().lines.len(), 1);
        inspector.request_refresh();
        assert!(inspector.state.refresh.take_pending().is_none());

        inspector.apply_refresh(DetailRefreshCompletion {
            ticket: current,
            result: DetailRefreshResult::Noop,
        });
        assert!(inspector.state.refresh.take_pending().is_some());
    }
}
