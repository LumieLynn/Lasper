//! Route component events and overlay commands to their App integrations.
//!
//! The core owns polling and input priority. This is the explicit junction
//! between that machinery and the concrete feature coordination modules.

use crate::tui::app::core::overlay::OverlayState;
use crate::tui::app::core::OverlayCommand;
use crate::tui::app::App;
use crate::tui::protocol::events::AppEvent;

impl App {
    pub(in crate::tui::app) async fn dispatch_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key).await,
            AppEvent::Mouse(mouse) => self.handle_mouse(mouse).await,
            AppEvent::Resize => {}
            AppEvent::Tick => self.tick().await,
            AppEvent::ConfigurationInspected {
                query,
                target,
                result,
            } => self.finish_configuration_query(query, target, result),
            AppEvent::ConfigurationPreviewed {
                generation,
                target,
                result,
            } => self.finish_configuration_preview(generation, target, result),
            AppEvent::ConfigurationApplied {
                generation,
                target,
                result,
            } => self.finish_configuration_apply(generation, target, result),
            AppEvent::ConfigurationPage(event) => {
                self.handle_configuration_page_event(event);
            }
            AppEvent::Wizard(event) => self.handle_wizard_event(event).await,
            AppEvent::ImageRemovalFinished(completion) => {
                self.handle_image_removal_completion(completion)
            }
            AppEvent::MachineOperationFinished(outcome) => {
                self.handle_machine_operation_result(outcome)
            }
            AppEvent::MetricsUpdate(name, time_x, cpu, ram) => self
                .workspace
                .resources
                .catalog
                .update_metrics(name, time_x, cpu, ram),
            AppEvent::TerminalRedraw => self.terminal.clear_redraw_pending(),
        }
    }

    pub(in crate::tui::app) async fn apply_overlay_command(&mut self, command: OverlayCommand) {
        match command {
            OverlayCommand::Consumed => {}
            OverlayCommand::Configuration(action) => {
                self.handle_configuration_action(*action).await;
            }
            OverlayCommand::Wizard(action) => {
                self.handle_wizard_action(*action).await;
            }
            OverlayCommand::Resource(action) => self.handle_resource_command(action),
            OverlayCommand::LeaderShell => self.spawn_shell_prompt().await,
            OverlayCommand::LeaderLogin => self.spawn_terminal().await,
            OverlayCommand::LeaderX11 => self.spawn_x11_shell_prompt().await,
            OverlayCommand::LeaderConfigure => self.configure_focused_resource(),
            OverlayCommand::RemoveImage => self.action_remove(),
            OverlayCommand::Quit => {
                self.terminal.cleanup_all();
                self.signal_quit();
            }
        }
    }

    pub(in crate::tui::app) async fn shutdown_components(&mut self) {
        self.close_wizard().await;
        self.ui.overlays = OverlayState::new();
        self.workspace.inspector.shutdown().await;
        self.terminal.cleanup_all();
    }
}
