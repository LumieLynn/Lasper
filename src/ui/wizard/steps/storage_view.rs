use crate::nspawn::adapters::storage::StorageType;
use crate::nspawn::models::{DiskImageFilesystem, DiskImagePartition};
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::path_box::{expand_user_path, PathBox};
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{StorageState, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};

use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::Paragraph,
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let is_disk = $self.is_disk_image_selected();
        let creation_method_idx = $self.creation_method.selected_idx();

        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.list];
        if is_disk {
            comps.push(&mut $self.creation_method);
            if creation_method_idx == 0 {
                // Create New
                comps.push(&mut $self.disk_size);
                comps.push(&mut $self.disk_fs);
                comps.push(&mut $self.partition_table);
            } else {
                // Import
                comps.push(&mut $self.import_path);
                comps.push(&mut $self.root_partition_choices);
            }
        }
        comps
    }};
}

impl_wizard_nav!(StorageStepView, active_comps);

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootPartitionChoice {
    partition: Option<DiskImagePartition>,
    label: String,
}

impl RootPartitionChoice {
    fn automatic() -> Self {
        Self {
            partition: None,
            label: "Auto-detect".into(),
        }
    }

    fn selected(partition: DiskImagePartition, type_label: impl Into<String>) -> Self {
        Self {
            partition: Some(partition),
            label: format!("p{}  {}", partition.number(), type_label.into()),
        }
    }
}

pub struct StorageStepView {
    list: SelectableList<(StorageType, bool)>,
    creation_method: RadioGroup,
    disk_size: TextBox,
    disk_fs: SelectableList<(DiskImageFilesystem, bool)>,
    partition_table: Checkbox,
    import_path: PathBox,
    root_partition_choices: SelectableList<RootPartitionChoice>,
    focus: FocusTracker,
}

impl StorageStepView {
    pub fn new(initial_data: &StorageState) -> Self {
        let info = initial_data.info.clone();
        let types = info.types.clone();

        let mut list = SelectableList::new(" Storage Options ", types, |(st, supported)| {
            let status = if *supported { "" } else { " (unsupported)" };
            format!("{}{}", st.label(), status)
        })
        .with_item_enablement(|(_, supported)| *supported);

        // Ensure the initial selection is supported. If not, find the first supported one.
        let mut selected_idx = initial_data.type_idx;
        if let Some((_, supported)) = info.types.get(selected_idx) {
            if !*supported {
                selected_idx = info
                    .types
                    .iter()
                    .position(|(_, supported)| *supported)
                    .unwrap_or(0);
            }
        }

        list.select(selected_idx);

        let fs_options: Vec<(DiskImageFilesystem, bool)> = DiskImageFilesystem::ALL
            .iter()
            .map(|fs| {
                (
                    *fs,
                    crate::nspawn::ops::provision::builders::image::check_tool(fs.mkfs_tool())
                        .is_ok(),
                )
            })
            .collect();
        let mut disk_fs = SelectableList::new(" Filesystem ", fs_options, |(fs, supported)| {
            if *supported {
                fs.label().to_string()
            } else {
                format!("{} (missing {})", fs.label(), fs.mkfs_tool())
            }
        })
        .with_item_enablement(|(_, supported)| *supported);
        let mut fs_idx = initial_data.disk_fs.to_index();
        if let Some((_, supported)) = disk_fs.items().get(fs_idx) {
            if !*supported {
                fs_idx = disk_fs
                    .items()
                    .iter()
                    .position(|(_, supported)| *supported)
                    .unwrap_or(fs_idx);
            }
        }
        disk_fs.select(fs_idx);

        let mut initial_root_choices = vec![RootPartitionChoice::automatic()];
        if let Some(partition) = initial_data.disk_root_partition {
            initial_root_choices.push(RootPartitionChoice::selected(partition, "Selected"));
        }
        let mut root_partition_choices = SelectableList::new(
            " Root Partition ",
            initial_root_choices,
            |choice: &RootPartitionChoice| choice.label.clone(),
        );
        if initial_data.disk_root_partition.is_some() {
            root_partition_choices.select(1);
        }

        let mut view = Self {
            list,
            creation_method: RadioGroup::new(
                " Creation Method ",
                vec!["Create New Image".into(), "Import Existing Image".into()],
                initial_data.creation_method_idx,
            ),
            disk_size: TextBox::new(
                " Disk Volume Size (e.g. 2G, 500M) ",
                initial_data.disk_size.clone(),
            )
            .with_validator(|v| {
                crate::nspawn::models::config::parse_disk_image_size(v)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }),
            disk_fs,
            partition_table: Checkbox::new(" GPT Partition Table ", initial_data.disk_partition),
            import_path: PathBox::new(" Source Image Path ", initial_data.import_path.clone())
                .with_validator(|value| {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        return Err("Source image path required".into());
                    }
                    let path = expand_user_path(trimmed)?;
                    let metadata = std::fs::metadata(&path)
                        .map_err(|_| "Source image path does not exist".to_string())?;
                    let file_type = metadata.file_type();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::FileTypeExt;
                        if file_type.is_file() || file_type.is_block_device() {
                            return Ok(());
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        if file_type.is_file() {
                            return Ok(());
                        }
                    }
                    Err("Source image path must be a file or block device".into())
                }),
            root_partition_choices,
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }

    fn is_disk_image_selected(&self) -> bool {
        if let Some((st, _)) = self.list.selected_item() {
            return *st == StorageType::DiskImage;
        }
        false
    }

    fn selected_root_partition(&self) -> Option<DiskImagePartition> {
        self.root_partition_choices
            .selected_item()
            .and_then(|choice| choice.partition)
    }

    fn root_partition_list_height(&self) -> u16 {
        self.root_partition_choices.items().len().clamp(1, 6) as u16 + 2
    }

    fn refresh_root_partition_choices(&mut self, path: &std::path::Path) -> Result<(), String> {
        let probe = crate::nspawn::adapters::storage::image_ops::probe_image_partitions(path)
            .map_err(|error| error.to_string())?;
        self.apply_root_partition_probe(probe)
    }

    fn apply_root_partition_probe(
        &mut self,
        probe: Option<crate::nspawn::adapters::storage::image_ops::ImagePartitionProbe>,
    ) -> Result<(), String> {
        let previous = self.selected_root_partition();

        let mut choices = vec![RootPartitionChoice::automatic()];
        if let Some(probe) = &probe {
            choices.extend(probe.partitions.iter().map(|partition| {
                RootPartitionChoice::selected(
                    partition.number,
                    crate::nspawn::adapters::storage::image_ops::partition_type_label(
                        &partition.type_id,
                    ),
                )
            }));
        }
        self.root_partition_choices.set_items(choices);

        if let Some(previous) = previous {
            let selected = self
                .root_partition_choices
                .items()
                .iter()
                .position(|choice| choice.partition == Some(previous))
                .ok_or_else(|| {
                    format!(
                        "Previously selected root partition p{} no longer exists",
                        previous.number()
                    )
                })?;
            self.root_partition_choices.select(selected);
        } else {
            self.root_partition_choices.select(0);
        }

        if let Some(probe) = probe {
            let mut roots = Vec::new();
            for partition in &probe.partitions {
                if crate::nspawn::adapters::storage::image_ops::is_current_architecture_root_type(
                    &partition.type_id,
                )
                .map_err(|error| error.to_string())?
                {
                    roots.push(partition.number);
                }
            }

            if let Some(selected) = self.selected_root_partition() {
                if probe.partitions.len() > 1 && !probe.label.eq_ignore_ascii_case("gpt") {
                    return Err(
                        "Manual root selection for a multi-partition image requires GPT".into(),
                    );
                }
                match roots.as_slice() {
                    [] => {}
                    [root] if *root == selected => {}
                    [root] => {
                        return Err(format!(
                            "p{} is already marked as root; select it or use Auto-detect",
                            root.number()
                        ));
                    }
                    _ => {
                        return Err(
                            "Multiple root partitions are marked for this architecture".into()
                        );
                    }
                }
            } else if probe.partitions.len() > 1 {
                match roots.len() {
                    1 => {}
                    0 => {
                        return Err(
                            "No root partition is marked for this architecture; select one from the list"
                                .into(),
                        );
                    }
                    _ => {
                        return Err(
                            "Multiple root partitions are marked for this architecture".into()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

impl Component for StorageStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let is_disk = self.is_disk_image_selected();
        let is_import = self.creation_method.selected_idx() == 1;

        let mut constraints = vec![
            Constraint::Length(1), // Title
            Constraint::Min(0),    // List
        ];
        if is_disk {
            constraints.push(Constraint::Length(3)); // Creation Method
            if is_import {
                constraints.push(Constraint::Length(3)); // Import Path
                constraints.push(Constraint::Length(self.root_partition_list_height()));
            } else {
                constraints.push(Constraint::Length(3)); // Size
                constraints.push(Constraint::Length(
                    DiskImageFilesystem::ALL.len() as u16 + 2,
                )); // FS
                constraints.push(Constraint::Length(3)); // Partition table
            }
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        f.render_widget(
            Paragraph::new("Select storage backend for the container rootfs:"),
            chunks[0],
        );

        self.update_focus();

        self.list.render(f, chunks[1]);

        if is_disk {
            let mut current = 2;
            self.creation_method.render(f, chunks[current]);
            current += 1;

            if is_import {
                self.import_path.render(f, chunks[current]);
                current += 1;
                self.root_partition_choices.render(f, chunks[current]);
            } else {
                self.disk_size.render(f, chunks[current]);
                current += 1;
                self.disk_fs.render(f, chunks[current]);
                current += 1;
                self.partition_table.render(f, chunks[current]);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == crossterm::event::KeyCode::Tab
            && self.import_path.is_focused()
            && self.import_path.validate().is_ok()
        {
            if let Ok(path) = expand_user_path(self.import_path.value()) {
                let _ = self.refresh_root_partition_choices(&path);
            }
        }
        let res = delegate_wizard_navigation!(self, key, active_comps);

        if let EventResult::Consumed = res {
            if self.focus.active_idx == 0 {
                self.update_focus();
            }
        }
        res
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn validate(&mut self) -> Result<(), String> {
        if let Some((_, supported)) = self.list.selected_item() {
            if !*supported {
                return Err("Selected storage backend is not supported on this host".into());
            }
        }

        if self.is_disk_image_selected() {
            if self.creation_method.selected_idx() == 0 {
                self.disk_size.validate()?;
                let Some((fs, supported)) = self.disk_fs.selected_item() else {
                    return Err("Filesystem required".into());
                };
                if !*supported {
                    return Err(format!("Missing dependency: {}", fs.mkfs_tool()));
                }
                if self.partition_table.checked() {
                    for tool in ["sfdisk", "losetup", "udevadm"] {
                        crate::nspawn::ops::provision::builders::image::check_tool(tool)
                            .map_err(|_| format!("Missing dependency: {}", tool))?;
                    }
                }
            } else {
                self.import_path.validate()?;
                crate::nspawn::ops::provision::builders::image::check_tool("sfdisk")
                    .map_err(|_| "Missing dependency: sfdisk".to_string())?;
                let path = expand_user_path(self.import_path.value())?;
                self.refresh_root_partition_choices(&path)?;
            }
        }
        Ok(())
    }
}

impl StepComponent for StorageStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        if let Some(idx) = self.list.selected_idx() {
            ctx.storage.type_idx = idx;
        }
        ctx.storage.creation_method_idx = self.creation_method.selected_idx();
        ctx.storage.disk_size = self.disk_size.value().to_string();
        if let Some((fs, _)) = self.disk_fs.selected_item() {
            ctx.storage.disk_fs = *fs;
        }
        ctx.storage.disk_partition = self.partition_table.checked();
        ctx.storage.import_path = expand_user_path(self.import_path.value())
            .unwrap_or_else(|_| self.import_path.value().trim().into())
            .to_string_lossy()
            .into_owned();
        ctx.storage.disk_root_partition = self.selected_root_partition();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::adapters::storage::image_ops::{ImagePartitionInfo, ImagePartitionProbe};
    use crate::nspawn::adapters::storage::StorageInfo;

    fn state() -> StorageState {
        StorageState {
            type_idx: 0,
            info: StorageInfo {
                types: vec![(StorageType::DiskImage, true)],
            },
            creation_method_idx: 1,
            disk_size: "2G".into(),
            disk_fs: DiskImageFilesystem::Ext4,
            disk_partition: true,
            import_path: "/tmp/test.raw".into(),
            disk_root_partition: None,
        }
    }

    fn partition(number: u32, type_id: &str) -> ImagePartitionInfo {
        ImagePartitionInfo {
            number: DiskImagePartition::new(number).unwrap(),
            type_id: type_id.into(),
        }
    }

    #[test]
    fn partition_probe_populates_choices_and_preserves_manual_selection() {
        let probe = ImagePartitionProbe {
            label: "gpt".into(),
            partitions: vec![
                partition(1, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
                partition(2, "0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
            ],
        };
        let mut view = StorageStepView::new(&state());

        let error = view
            .apply_root_partition_probe(Some(probe.clone()))
            .unwrap_err();
        assert!(error.contains("select one from the list"));
        assert_eq!(view.root_partition_choices.items().len(), 3);

        view.root_partition_choices.select(2);
        view.apply_root_partition_probe(Some(probe)).unwrap();
        assert_eq!(view.selected_root_partition().unwrap().number(), 2);
    }

    #[test]
    fn partition_choice_list_has_a_stable_maximum_height() {
        let probe = ImagePartitionProbe {
            label: "gpt".into(),
            partitions: (1..=12)
                .map(|number| partition(number, "0FC63DAF-8483-4772-8E79-3D69D8477DE4"))
                .collect(),
        };
        let mut view = StorageStepView::new(&state());
        let _ = view.apply_root_partition_probe(Some(probe));

        assert_eq!(view.root_partition_choices.items().len(), 13);
        assert_eq!(view.root_partition_list_height(), 8);
    }
}
