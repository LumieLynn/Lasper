//! Borrowed data exposed to detail-panel rendering.
//!
//! The application owns the live state.  The detail panel only needs this
//! narrow view while it measures and paints the active pane, so renderers do
//! not depend on the application controller or its service handles.

use std::collections::VecDeque;
use std::path::Path;

use crate::application::inspection::SystemdDropInInspection;
use crate::domain::inspection::MachineProperties;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::tui::features::workspace::inspector::core::model::DetailTarget;
use crate::tui::features::workspace::resources::metrics::MachineMetrics;

pub(crate) struct DetailRenderContext<'a> {
    pub(crate) target: &'a DetailTarget,
    pub(crate) machine: Option<&'a MachineEntry>,
    pub(crate) image: Option<&'a ImageEntry>,
    pub(crate) metrics: Option<&'a MachineMetrics>,
    pub(crate) properties: Result<&'a MachineProperties, &'a str>,
    pub(crate) config_content: Option<&'a str>,
    pub(crate) config_path: Option<&'a Path>,
    pub(crate) config_error: Option<&'a str>,
    pub(crate) unit_name: Option<&'a str>,
    pub(crate) unit_drop_ins: &'a [SystemdDropInInspection],
    pub(crate) logs: Option<DetailLogData<'a>>,
    pub(crate) overview_dirty: &'a mut bool,
    pub(crate) config_dirty: &'a mut bool,
    pub(crate) details_dirty: &'a mut bool,
}

/// Presentation can acknowledge new lines without controlling their stream.
pub(crate) struct DetailLogData<'a> {
    pub(crate) lines: &'a VecDeque<String>,
    pub(crate) dirty: &'a mut bool,
}
