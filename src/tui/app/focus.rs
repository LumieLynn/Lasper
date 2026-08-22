//! Semantic focus state for the main workspace.
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

    /// Keep the current inspector context when a click targets the detail
    /// panel.  The detail panel is one visible panel but has two semantic
    /// focus destinations.
    pub const fn for_panel(current: Self, panel_index: usize) -> Self {
        match panel_index {
            0 => Self::Machines,
            1 => Self::Images,
            2 => match current {
                Self::ImageInspector | Self::Images => Self::ImageInspector,
                _ => Self::MachineInspector,
            },
            3 => Self::Terminal,
            _ => current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceFocus;

    #[test]
    fn inspector_focus_keeps_image_context_when_clicking_detail() {
        assert_eq!(
            WorkspaceFocus::for_panel(WorkspaceFocus::Images, 2),
            WorkspaceFocus::ImageInspector
        );
        assert_eq!(
            WorkspaceFocus::for_panel(WorkspaceFocus::ImageInspector, 2),
            WorkspaceFocus::ImageInspector
        );
        assert_eq!(
            WorkspaceFocus::for_panel(WorkspaceFocus::Machines, 2),
            WorkspaceFocus::MachineInspector
        );
    }
}
