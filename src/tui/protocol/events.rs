//! Notifications from feature tasks and host input to the application loop.

use crossterm::event::{KeyEvent, MouseEvent};

/// Events the main loop handles.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    Tick,
    ConfigurationInspected {
        query: u64,
        target: crate::application::configuration::ConfigurationTarget,
        result: Result<
            crate::application::configuration::ConfigurationSnapshot,
            crate::application::configuration::ConfigurationError,
        >,
    },
    ConfigurationPreviewed {
        generation: u64,
        target: crate::application::configuration::ConfigurationTarget,
        result: Result<
            crate::application::configuration::ConfigurationPreview,
            crate::application::configuration::ConfigurationError,
        >,
    },
    ConfigurationApplied {
        generation: u64,
        target: crate::application::configuration::ConfigurationTarget,
        result: Result<
            crate::application::configuration::ConfigurationApplyReport,
            crate::application::configuration::ConfigurationError,
        >,
    },
    ConfigurationPage(crate::tui::features::configuration::ConfigurationPageEvent),
    Wizard(crate::tui::features::wizard::workflow::WizardEvent),
    /// Image removal completed independently of the confirmation overlay.
    ImageRemovalFinished(
        crate::tui::features::workspace::resources::actions::ImageRemovalCompletion,
    ),
    /// Operation execution returned; machine state is delivered by runtime snapshots.
    MachineOperationFinished(crate::application::MachineLifecycleOutcome),
    /// Real-time metrics: (machine name, timestamp, CPU percent, RAM MiB)
    MetricsUpdate(String, f64, f64, f64),
    /// Request a UI redraw for the terminal.
    TerminalRedraw,
}

impl AppEvent {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Key(_) => "key",
            Self::Mouse(_) => "mouse",
            Self::Resize => "resize",
            Self::Tick => "tick",
            Self::ConfigurationInspected { .. } => "configuration-inspected",
            Self::ConfigurationPreviewed { .. } => "configuration-previewed",
            Self::ConfigurationApplied { .. } => "configuration-applied",
            Self::ConfigurationPage(event) => event.label(),
            Self::Wizard(event) => event.label(),
            Self::ImageRemovalFinished(_) => "image-removal-finished",
            Self::MachineOperationFinished(_) => "machine-operation-finished",
            Self::MetricsUpdate(_, _, _, _) => "metrics-update",
            Self::TerminalRedraw => "terminal-redraw",
        }
    }
}
