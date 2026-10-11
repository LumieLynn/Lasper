//! Semantic focus destinations and navigation history for the workspace.
//!
//! The main screen has more focusable destinations than it has visible
//! panels: each list has a corresponding inspector destination and the
//! terminal is optional.  Keeping that distinction in the focus value avoids
//! making callers reconstruct it from a numeric index plus a second
//! `InspectorSource` field.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceFocus {
    #[default]
    Machines,
    MachineInspector,
    Images,
    ImageInspector,
    Terminal,
}

impl WorkspaceFocus {
    pub const fn is_machine_list(self) -> bool {
        matches!(self, Self::Machines)
    }

    pub const fn is_image_list(self) -> bool {
        matches!(self, Self::Images)
    }

    pub const fn is_inspector(self) -> bool {
        matches!(self, Self::MachineInspector | Self::ImageInspector)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }
}

/// Focus history belongs to the workspace, independently of its panels.
#[derive(Default)]
pub(super) struct WorkspaceNavigation {
    current: WorkspaceFocus,
    previous_non_terminal: WorkspaceFocus,
}

impl WorkspaceNavigation {
    pub(super) fn current(&self) -> WorkspaceFocus {
        self.current
    }

    pub(super) fn set_focus(&mut self, focus: WorkspaceFocus) -> bool {
        let changed = self.current != focus;
        if !focus.is_terminal() {
            self.previous_non_terminal = focus;
        }
        self.current = focus;
        changed
    }

    pub(super) fn non_terminal_destination(&self) -> WorkspaceFocus {
        self.previous_non_terminal
    }

    pub(super) fn next_focus(
        from: WorkspaceFocus,
        forward: bool,
        terminal_showing: bool,
        terminal_maximized: bool,
    ) -> WorkspaceFocus {
        const WITHOUT_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::MachineInspector,
            WorkspaceFocus::Images,
            WorkspaceFocus::ImageInspector,
        ];
        const WITH_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::MachineInspector,
            WorkspaceFocus::Images,
            WorkspaceFocus::ImageInspector,
            WorkspaceFocus::Terminal,
        ];
        const MAXIMIZED_TERMINAL: &[WorkspaceFocus] = &[
            WorkspaceFocus::Machines,
            WorkspaceFocus::Images,
            WorkspaceFocus::Terminal,
        ];
        let slots = if terminal_showing && terminal_maximized {
            MAXIMIZED_TERMINAL
        } else if terminal_showing {
            WITH_TERMINAL
        } else {
            WITHOUT_TERMINAL
        };
        let index = slots.iter().position(|slot| *slot == from).unwrap_or(0);
        if forward {
            slots[(index + 1) % slots.len()]
        } else {
            slots[(index + slots.len() - 1) % slots.len()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceFocus, WorkspaceNavigation};

    #[test]
    fn focus_cycle_changes_when_terminal_is_maximized() {
        assert_eq!(
            WorkspaceNavigation::next_focus(WorkspaceFocus::Images, true, true, true),
            WorkspaceFocus::Terminal
        );
        assert_eq!(
            WorkspaceNavigation::next_focus(WorkspaceFocus::MachineInspector, true, true, true),
            WorkspaceFocus::Images
        );
    }
}
