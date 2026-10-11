//! Targets and navigation policy for the detail-panel coordinator.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DetailTarget {
    #[default]
    Empty,
    Machine(String),
    Image {
        name: String,
        internal: bool,
    },
}

impl DetailTarget {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Empty => None,
            Self::Machine(name) | Self::Image { name, .. } => Some(name),
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    pub fn is_internal_image(&self) -> bool {
        matches!(self, Self::Image { internal: true, .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPane {
    MachineOverview,
    Details,
    Logs,
    Config,
    Metrics,
    ImageOverview,
    ImageConfig,
}

impl DetailPane {
    pub const MACHINE: &[DetailPane] = &[
        DetailPane::MachineOverview,
        DetailPane::Details,
        DetailPane::Logs,
        DetailPane::Config,
        DetailPane::Metrics,
    ];

    pub const IMAGE: &[DetailPane] = &[
        DetailPane::ImageOverview,
        DetailPane::ImageConfig,
        DetailPane::Details,
    ];
    pub const INTERNAL_IMAGE: &[DetailPane] = &[DetailPane::ImageOverview];

    pub fn tabs_for(target: &DetailTarget) -> &'static [DetailPane] {
        match target {
            DetailTarget::Image { internal: true, .. } => Self::INTERNAL_IMAGE,
            DetailTarget::Image {
                internal: false, ..
            } => Self::IMAGE,
            DetailTarget::Empty | DetailTarget::Machine(_) => Self::MACHINE,
        }
    }

    pub fn next_for(&self, target: &DetailTarget) -> Self {
        let tabs = Self::tabs_for(target);
        let idx = tabs.iter().position(|pane| pane == self).unwrap_or(0);
        tabs[(idx + 1) % tabs.len()]
    }

    pub fn prev_for(&self, target: &DetailTarget) -> Self {
        let tabs = Self::tabs_for(target);
        let idx = tabs.iter().position(|pane| pane == self).unwrap_or(0);
        tabs[(idx + tabs.len() - 1) % tabs.len()]
    }

    pub fn from_index(idx: usize, target: &DetailTarget) -> Option<Self> {
        Self::tabs_for(target).get(idx).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailPaneContext {
    Machine,
    Image,
    InternalImage,
}

impl DetailPaneContext {
    pub(crate) fn for_target(target: &DetailTarget) -> Self {
        match target {
            DetailTarget::Image { internal: true, .. } => Self::InternalImage,
            DetailTarget::Image {
                internal: false, ..
            } => Self::Image,
            DetailTarget::Empty | DetailTarget::Machine(_) => Self::Machine,
        }
    }
}
