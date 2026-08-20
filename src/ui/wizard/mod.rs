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
    HostIntegration,
    BindMounts,
    Review,
    Deploy,
}

pub enum StepAction {
    None,
    Next,
    Prev,
    Close,
    Status(String, crate::ui::StatusLevel),
    OpenDialog(Box<dyn crate::ui::core::Component>),
    CloseDialog,
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
            WizardStep::HostIntegration => "Host Integration",
            WizardStep::BindMounts => "Bind Mounts",
            WizardStep::Review => "Final Review",
            WizardStep::Deploy => "Deployment Progress",
        }
    }
}
