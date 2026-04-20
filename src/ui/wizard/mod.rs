#[macro_use]
pub mod core;
pub use self::core::context;
pub use self::core::manager;
pub mod steps;

pub use self::manager::Wizard;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WizardStep {
    Source,
    CopySelect,
    Basic,
    Storage,
    User,
    Network,
    Passthrough,
    Devices,
    Review,
    Deploy,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepAction {
    None,
    Next,
    Prev,
    Close,
    /// Display a status message in the application status bar.
    Status(String, crate::ui::StatusLevel),
}

impl WizardStep {
    pub fn title(&self) -> &str {
        match self {
            WizardStep::Source => "Source Selection",
            WizardStep::CopySelect => "Select Image to Clone",
            WizardStep::Basic => "Basic Configuration",
            WizardStep::Storage => "Storage Settings",
            WizardStep::User => "User Management",
            WizardStep::Network => "Network Configuration",
            WizardStep::Passthrough => "Hardware Passthrough",
            WizardStep::Devices => "Device & Mounts",
            WizardStep::Review => "Final Review",
            WizardStep::Deploy => "Deployment Progress",
        }
    }
}
