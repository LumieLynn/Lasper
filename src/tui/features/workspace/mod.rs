//! Main-screen feature: resource browsing, inspection and terminal panels.
//!
//! `core` owns workspace composition and transitions between panels. Each
//! panel owns its data, controls and internal layout. App supplies runtime
//! updates and terminal sessions, and applies commands leaving this feature.

mod core;
pub(crate) mod inspector;
pub(crate) mod overlays;
pub(crate) mod resources;
pub(crate) mod terminal_panel;

pub(crate) use core::{Workspace, WorkspaceCommand, WorkspaceFocus, WorkspaceInputResult};
