//! State management and logic for the container creation wizard.

use ratatui::{layout::Rect, Frame};
use crossterm::event::KeyEvent;
use crate::nspawn::StatusLevel;

/// The resulting action from handling a key event in a wizard step.
#[derive(Debug, Clone, PartialEq)]
pub enum StepAction {
    /// No action needed.
    None,
    /// Move to the next step.
    Next,
    /// Move back to the previous step.
    Prev,
    /// Close the wizard overlay entirely.
    Close,
    /// Close the wizard and trigger a refresh of the container list.
    CloseRefresh,
    /// Display a status message in the application status bar.
    Status(String, StatusLevel),
}

use async_trait::async_trait;

/// A trait representing a single step in the multi-step container creation wizard.
#[async_trait]
pub trait IStep {
    /// The human-readable title of this step (shown in the wizard header).
    fn title(&self) -> String;
    
    /// Renders this step's UI within the given area.
    fn render(&mut self, f: &mut Frame, area: Rect, context: &self::context::WizardContext);
    
    /// Processes a key event for this step and returns the resulting action.
    async fn handle_key(&mut self, key: KeyEvent, context: &mut self::context::WizardContext) -> StepAction;
}

pub mod steps;
pub mod context;
pub mod manager;

pub use self::context::*;
pub use self::manager::*;
