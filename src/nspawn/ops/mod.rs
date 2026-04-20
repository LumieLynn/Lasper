pub mod handlers;
pub mod inspect;
pub mod manager;
pub mod provision;

pub use manager::{DefaultManager, NspawnManager};

#[derive(Debug, Clone)]
pub enum BackendCommand {
    SubmitConfig(Box<crate::ui::wizard::context::WizardContext>),
    ValidateInterface { name: String, is_bridge_mode: bool },
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum BackendResponse {
    ValidationSuccess,
    ValidationError(String),
    ValidationWarning(String),
    DeployStarted,
    DeployFailed(String),
}
