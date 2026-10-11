//! Shared workspace policy and the objects that enforce it.
//!
//! The manager owns panel composition and commits local input transitions.
//! Navigation owns focus history; layout owns proportions, resizing and the
//! geometry shared by drawing and hit-testing. Input and footer registration
//! use the same shortcut definitions. Panel-specific controls, data and work
//! remain in resources, inspector and terminal_panel.

mod commands;
mod focus;
mod footer;
mod input;
mod layout;
mod manager;
mod presentation;
mod shortcuts;

pub(crate) use commands::{WorkspaceCommand, WorkspaceInputResult};
pub(crate) use focus::WorkspaceFocus;
pub(crate) use manager::Workspace;
