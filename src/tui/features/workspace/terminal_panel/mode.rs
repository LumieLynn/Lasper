//! Interaction modes owned by the embedded terminal panel.

/// The panel's input mode is separate from the terminal application's own
/// mode. Insert forwards input to the PTY; the other modes are local browser
/// interactions and never send their navigation keys to the guest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMode {
    Insert,
    Normal,
    Visual(VisualSelectionMode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualSelectionMode {
    Character,
    Line,
    Block,
}

impl TerminalMode {
    #[must_use]
    pub const fn is_insert(self) -> bool {
        matches!(self, Self::Insert)
    }

    #[must_use]
    pub const fn is_browser(self) -> bool {
        !self.is_insert()
    }

    #[must_use]
    pub const fn is_normal(self) -> bool {
        matches!(self, Self::Normal)
    }

    #[must_use]
    pub const fn is_visual(self) -> bool {
        matches!(self, Self::Visual(_))
    }
}
