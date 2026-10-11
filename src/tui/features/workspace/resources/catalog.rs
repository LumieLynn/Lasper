//! Observable machine/image state used by the main TUI workspace.

use crate::domain::runtime::{ImageEntry, MachineEntry};
use crate::tui::features::workspace::resources::metrics::{CpuRepresentation, MachineMetrics};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceListKind {
    Machines,
    Images,
    InternalImages,
}

pub(crate) struct ResourceCatalog {
    /// Running systemd-machined instances plus optimistic `Starting` rows.
    pub(crate) entries: Vec<MachineEntry>,
    /// Normally visible images. Hidden dot-prefixed images are kept separate.
    pub(crate) images: Vec<ImageEntry>,
    pub(crate) internal_images: Vec<ImageEntry>,
    pub(crate) image_selected: usize,
    pub(crate) internal_image_selected: usize,
    pub(crate) selected: usize,
    pub(crate) dbus_active: bool,
    pub(crate) metrics: HashMap<String, MachineMetrics>,
    pub(crate) cpu_cores: usize,
    pub(crate) cpu_representation: CpuRepresentation,
}

impl ResourceCatalog {
    pub(crate) fn new(systemd_tools: bool) -> Self {
        Self {
            entries: Vec::new(),
            images: Vec::new(),
            internal_images: Vec::new(),
            image_selected: 0,
            internal_image_selected: 0,
            selected: 0,
            dbus_active: !systemd_tools,
            metrics: HashMap::new(),
            cpu_cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            cpu_representation: CpuRepresentation::Normalized,
        }
    }

    /// Apply an observed machine list while keeping the selection on its name.
    /// Return whether the visible entries changed.
    pub(crate) fn replace_entries(&mut self, entries: Vec<MachineEntry>) -> bool {
        let selected_name = self
            .entries
            .get(self.selected)
            .map(|entry| entry.name.as_str());
        let selected = selected_name
            .and_then(|name| entries.iter().position(|entry| entry.name == name))
            .unwrap_or(0);
        let changed = self.entries != entries;
        self.entries = entries;
        self.selected = selected;
        changed
    }

    /// Let the application project lifecycle transitions onto the existing
    /// list without cloning it or losing the selected name during the move.
    pub(crate) fn project_entries(
        &mut self,
        project: impl FnOnce(Vec<MachineEntry>) -> Vec<MachineEntry>,
    ) {
        let selected_name = self
            .entries
            .get(self.selected)
            .map(|entry| entry.name.clone());
        self.entries = project(std::mem::take(&mut self.entries));
        self.selected = selected_name
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .unwrap_or(0);
    }

    /// Split and sort the image catalog without coupling either selection to
    /// the ordering of the backend response.
    pub(crate) fn replace_images(&mut self, images: Vec<ImageEntry>) -> bool {
        let selected_name = self
            .images
            .get(self.image_selected)
            .map(|image| image.name.as_str());
        let internal_selected_name = self
            .internal_images
            .get(self.internal_image_selected)
            .map(|image| image.name.as_str());
        let (mut visible, mut internal): (Vec<_>, Vec<_>) =
            images.into_iter().partition(|image| !image.is_hidden());
        visible.sort();
        internal.sort();
        let selected = selected_name
            .and_then(|name| visible.iter().position(|image| image.name == name))
            .unwrap_or(0);
        let internal_selected = internal_selected_name
            .and_then(|name| internal.iter().position(|image| image.name == name))
            .unwrap_or(0);
        let changed = visible != self.images || internal != self.internal_images;
        self.images = visible;
        self.internal_images = internal;
        self.image_selected = selected;
        self.internal_image_selected = internal_selected;
        changed
    }

    pub(crate) fn retain_metrics_for_entries(&mut self) {
        let active_names = self
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        self.metrics
            .retain(|name, _| active_names.contains(name.as_str()));
    }

    pub(crate) fn active_images(&self, internal: bool) -> (&[ImageEntry], usize) {
        if internal {
            (&self.internal_images, self.internal_image_selected)
        } else {
            (&self.images, self.image_selected)
        }
    }

    pub(crate) fn move_selection(&mut self, list: ResourceListKind, forward: bool) {
        let (length, selected) = match list {
            ResourceListKind::Machines => (self.entries.len(), &mut self.selected),
            ResourceListKind::Images => (self.images.len(), &mut self.image_selected),
            ResourceListKind::InternalImages => (
                self.internal_images.len(),
                &mut self.internal_image_selected,
            ),
        };
        if length == 0 {
            *selected = 0;
            return;
        }
        *selected = (*selected).min(length - 1);
        *selected = if forward {
            (*selected + 1) % length
        } else if *selected == 0 {
            length - 1
        } else {
            *selected - 1
        };
    }

    pub(crate) fn select_index(&mut self, list: ResourceListKind, index: usize) {
        let (length, selected) = match list {
            ResourceListKind::Machines => (self.entries.len(), &mut self.selected),
            ResourceListKind::Images => (self.images.len(), &mut self.image_selected),
            ResourceListKind::InternalImages => (
                self.internal_images.len(),
                &mut self.internal_image_selected,
            ),
        };
        *selected = index.min(length.saturating_sub(1));
    }

    pub(crate) fn select_machine_by_name(&mut self, name: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.name == name) {
            self.selected = index;
        }
    }

    pub(crate) fn update_metrics(&mut self, name: String, time_x: f64, cpu: f64, ram: f64) {
        let metrics = self.metrics.entry(name).or_default();
        metrics.cpu_history.push((time_x, cpu));
        metrics.ram_history.push((time_x, ram));
        if metrics.cpu_history.len() > 60 {
            metrics.cpu_history.remove(0);
        }
        if metrics.ram_history.len() > 60 {
            metrics.ram_history.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::MachineState;

    fn machine(name: &str) -> MachineEntry {
        MachineEntry::optimistic_nspawn(name, MachineState::Running)
    }

    #[test]
    fn projection_keeps_the_selected_machine_across_reordering_and_removal() {
        let mut runtime = ResourceCatalog::new(false);
        runtime.replace_entries(vec![machine("first"), machine("selected")]);
        runtime.selected = 1;

        runtime.project_entries(|mut entries| {
            entries.reverse();
            entries.push(machine("starting"));
            entries
        });
        assert_eq!(runtime.entries[runtime.selected].name, "selected");

        runtime.project_entries(|mut entries| {
            entries.retain(|entry| entry.name != "selected");
            entries
        });
        assert_eq!(runtime.entries[runtime.selected].name, "first");

        runtime.project_entries(|_| Vec::new());
        assert_eq!(runtime.selected, 0);
    }

    #[test]
    fn metrics_remain_bounded_and_are_released_for_removed_machines() {
        let mut runtime = ResourceCatalog::new(false);
        runtime.replace_entries(vec![machine("retained"), machine("removed")]);
        for time in 0..100 {
            runtime.update_metrics("retained".into(), time as f64, 1.0, 2.0);
            runtime.update_metrics("removed".into(), time as f64, 3.0, 4.0);
        }
        let retained = &runtime.metrics["retained"];
        assert_eq!(retained.cpu_history.len(), 60);
        assert_eq!(retained.ram_history.len(), 60);
        assert_eq!(retained.cpu_history.first().unwrap().0, 40.0);

        runtime.replace_entries(vec![machine("retained")]);
        runtime.retain_metrics_for_entries();
        assert_eq!(runtime.metrics.len(), 1);
        assert_eq!(runtime.metrics["retained"].cpu_history.len(), 60);

        runtime.replace_entries(Vec::new());
        runtime.retain_metrics_for_entries();
        assert!(runtime.metrics.is_empty());
    }
}
