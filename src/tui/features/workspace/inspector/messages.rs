//! Pane changes produced by the inspector's controls.

use crate::tui::features::workspace::inspector::DetailPane;

#[derive(Debug, Clone, PartialEq)]
pub enum ContainerMessage {
    PaneChanged(DetailPane),
}
