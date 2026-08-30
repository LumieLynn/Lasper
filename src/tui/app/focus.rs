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
}
