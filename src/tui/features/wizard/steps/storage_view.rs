use crate::application::provisioning::{
    ImagePartitionProbe, ProvisioningPreparationService, StorageBackendKind,
};
use crate::domain::storage::{parse_disk_image_size, DiskImageFilesystem, DiskImagePartition};
use crate::tui::features::wizard::core::input::expand_user_path;
use crate::tui::features::wizard::draft::{StorageState, WizardDraft};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::inputs::text_input::{Input, TextInput};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::foundation::widgets::selectors::{
    checkbox::CheckboxControl,
    radio::{RadioControl, RadioState},
};
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    widgets::Paragraph,
    Frame,
};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootPartitionChoice {
    partition: Option<DiskImagePartition>,
    label: String,
}

fn filesystem_label(filesystem: DiskImageFilesystem) -> &'static str {
    match filesystem {
        DiskImageFilesystem::Ext4 => "Ext4",
        DiskImageFilesystem::Xfs => "XFS",
        DiskImageFilesystem::Btrfs => "Btrfs",
    }
}

fn filesystem_tool(filesystem: DiskImageFilesystem) -> &'static str {
    match filesystem {
        DiskImageFilesystem::Ext4 => "mkfs.ext4",
        DiskImageFilesystem::Xfs => "mkfs.xfs",
        DiskImageFilesystem::Btrfs => "mkfs.btrfs",
    }
}

fn filesystem_index(filesystem: DiskImageFilesystem) -> usize {
    match filesystem {
        DiskImageFilesystem::Ext4 => 0,
        DiskImageFilesystem::Xfs => 1,
        DiskImageFilesystem::Btrfs => 2,
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageControl {
    Title,
    Backend,
    Method,
    Size,
    Filesystem,
    PartitionTable,
    ImportPath,
    RootPartition,
}
pub struct StorageStepView {
    types: Vec<(StorageBackendKind, bool)>,
    selected: ListControlState,
    creation_method: RadioState,
    disk_size: Input,
    size_error: Option<String>,
    filesystems: Vec<(DiskImageFilesystem, bool)>,
    fs_selection: ListControlState,
    partition_table: bool,
    import_path: Input,
    path_error: Option<String>,
    root_partition_choices: Vec<RootPartitionChoice>,
    root_selection: ListControlState,
    preparation: Arc<ProvisioningPreparationService>,
    tools: std::collections::BTreeMap<String, bool>,
    form: FormState<StorageControl>,
}
impl StorageStepView {
    pub fn new(
        initial: &StorageState,
        preparation: Arc<ProvisioningPreparationService>,
        tools: std::collections::BTreeMap<String, bool>,
    ) -> Self {
        let mut root_partition_choices = vec![RootPartitionChoice::automatic()];
        if let Some(partition) = initial.disk_root_partition {
            root_partition_choices.push(RootPartitionChoice::selected(partition, "Selected"));
        }
        let mut view = Self {
            types: initial.info.types.clone(),
            selected: ListControlState::new(initial.type_idx),
            creation_method: RadioState::new(initial.creation_method_idx),
            disk_size: Input::from(initial.disk_size.clone()),
            size_error: None,
            filesystems: initial.info.filesystems.clone(),
            fs_selection: ListControlState::new(filesystem_index(initial.disk_fs)),
            partition_table: initial.disk_partition,
            import_path: Input::from(initial.import_path.clone()),
            path_error: None,
            root_partition_choices,
            root_selection: ListControlState::new(usize::from(
                initial.disk_root_partition.is_some(),
            )),
            preparation,
            tools,
            form: FormState::new(StorageControl::Backend),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn is_disk_image_selected(&self) -> bool {
        self.selected
            .selected()
            .and_then(|index| self.types.get(index))
            .is_some_and(|(kind, _)| *kind == StorageBackendKind::DiskImage)
    }
    fn selected_root_partition(&self) -> Option<DiskImagePartition> {
        self.root_selection
            .selected()
            .and_then(|index| self.root_partition_choices.get(index))
            .and_then(|choice| choice.partition)
    }
    fn root_partition_list_height(&self) -> u16 {
        self.root_partition_choices.len().clamp(1, 6) as u16 + 2
    }
    fn validate_import_path(&mut self) -> Result<(), String> {
        let result = validate_image_path(self.import_path.value());
        self.path_error = result.as_ref().err().cloned();
        result
    }
    fn refresh_import_choices(&mut self) {
        if self.is_disk_image_selected()
            && self.creation_method.selected_idx() == 1
            && self.validate_import_path().is_ok()
        {
            if let Ok(path) = expand_user_path(self.import_path.value()) {
                let _ = self.refresh_root_partition_choices(&path);
            }
            self.with_form(|_| ());
        }
    }
    fn refresh_root_partition_choices(&mut self, path: &std::path::Path) -> Result<(), String> {
        let probe = self
            .preparation
            .probe_image(path)
            .map_err(|error| error.to_string())?;
        self.apply_root_partition_probe(probe)
    }
    fn apply_root_partition_probe(
        &mut self,
        probe: Option<ImagePartitionProbe>,
    ) -> Result<(), String> {
        let previous = self.selected_root_partition();

        let mut choices = vec![RootPartitionChoice::automatic()];
        if let Some(probe) = &probe {
            choices.extend(probe.partitions.iter().map(|partition| {
                RootPartitionChoice::selected(partition.number, partition.type_label.clone())
            }));
        }
        self.root_partition_choices = choices;

        if let Some(previous) = previous {
            let selected = self
                .root_partition_choices
                .iter()
                .position(|choice| choice.partition == Some(previous))
                .ok_or_else(|| {
                    format!(
                        "Previously selected root partition p{} no longer exists",
                        previous.number()
                    )
                })?;
            self.root_selection.select(selected);
        } else {
            self.root_selection.select(0);
        }

        if let Some(probe) = probe {
            let mut roots = Vec::new();
            for partition in &probe.partitions {
                if partition.current_architecture_root {
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
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, StorageControl>) -> R) -> R {
        // Reconcile supported selections before deciding which fields are visible.
        let type_entries: Vec<_> = self
            .types
            .iter()
            .map(|(kind, supported)| {
                ListEntry::new(format!(
                    "{}{}",
                    kind.label(),
                    if *supported { "" } else { " (unsupported)" }
                ))
                .with_enabled(*supported)
            })
            .collect();
        let fs_entries: Vec<_> = self
            .filesystems
            .iter()
            .map(|(filesystem, supported)| {
                ListEntry::new(if *supported {
                    filesystem_label(*filesystem).into()
                } else {
                    format!(
                        "{} (missing {})",
                        filesystem_label(*filesystem),
                        filesystem_tool(*filesystem)
                    )
                })
                .with_enabled(*supported)
            })
            .collect();
        let root_entries: Vec<_> = self
            .root_partition_choices
            .iter()
            .map(|choice| ListEntry::new(choice.label.as_str()))
            .collect();
        let root_height = self.root_partition_list_height();
        let mut backends = ListControl::new(&mut self.selected, " Storage Options ", &type_entries);
        let is_disk = backends_selected_disk(&self.types, backends.selected());
        let is_import = self.creation_method.selected_idx() == 1;
        let mut filesystems = ListControl::new(&mut self.fs_selection, " Filesystem ", &fs_entries);
        let mut roots =
            ListControl::new(&mut self.root_selection, " Root Partition ", &root_entries);
        let mut method = RadioControl::new(
            &mut self.creation_method,
            " Creation Method ",
            &["Create New Image", "Import Existing Image"],
        );
        let mut size = TextInput::new(
            &mut self.disk_size,
            " Disk Volume Size (e.g. 2G, 500M) ",
            self.size_error.as_deref(),
        );
        let mut import = TextInput::new(
            &mut self.import_path,
            " Source Image Path ",
            self.path_error.as_deref(),
        );
        let mut partition =
            CheckboxControl::new(&mut self.partition_table, " GPT Partition Table ");
        let mut title = ContentControl::new(Paragraph::new(
            "Select storage backend for the container rootfs:",
        ));
        let mut items = vec![
            FormItem::field(StorageControl::Title, &mut title).height(1),
            FormItem::field(StorageControl::Backend, &mut backends).fill(4),
        ];
        if is_disk {
            items.push(FormItem::field(StorageControl::Method, &mut method));
            if is_import {
                items.extend([
                    FormItem::field(StorageControl::ImportPath, &mut import),
                    FormItem::field(StorageControl::RootPartition, &mut roots).height(root_height),
                ]);
            } else {
                items.extend([
                    FormItem::field(StorageControl::Size, &mut size),
                    FormItem::field(StorageControl::Filesystem, &mut filesystems)
                        .height(DiskImageFilesystem::ALL.len() as u16 + 2),
                    FormItem::field(StorageControl::PartitionTable, &mut partition),
                ]);
            }
        }
        visit(&mut Form::new(&mut self.form, items))
    }
}
fn backends_selected_disk(types: &[(StorageBackendKind, bool)], selected: Option<usize>) -> bool {
    selected
        .and_then(|index| types.get(index))
        .is_some_and(|(kind, _)| *kind == StorageBackendKind::DiskImage)
}
fn validate_image_path(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Source image path required".into());
    }
    let path = expand_user_path(value)?;
    let metadata =
        std::fs::metadata(path).map_err(|_| "Source image path does not exist".to_string())?;
    use std::os::unix::fs::FileTypeExt;
    if metadata.is_file() || metadata.file_type().is_block_device() {
        Ok(())
    } else {
        Err("Source image path must be a file or block device".into())
    }
}
impl Component for StorageStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == KeyCode::Tab && self.form.is_selected(StorageControl::ImportPath) {
            self.refresh_import_choices();
        }
        field_event(self.with_form(|form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let was_import = self.form.is_selected(StorageControl::ImportPath);
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        if was_import && !self.form.is_selected(StorageControl::ImportPath) {
            self.refresh_import_choices();
        }
        field_event(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        let Some((_, supported)) = self
            .selected
            .selected()
            .and_then(|index| self.types.get(index))
        else {
            return Err("Storage backend required".into());
        };
        if !supported {
            return Err("Selected storage backend is not supported on this host".into());
        }
        if self.is_disk_image_selected() {
            if self.creation_method.selected_idx() == 0 {
                let result = parse_disk_image_size(self.disk_size.value())
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                self.size_error = result.as_ref().err().cloned();
                result?;
                let Some((filesystem, supported)) = self
                    .fs_selection
                    .selected()
                    .and_then(|index| self.filesystems.get(index))
                else {
                    return Err("Filesystem required".into());
                };
                if !supported {
                    return Err(format!(
                        "Missing dependency: {}",
                        filesystem_tool(*filesystem)
                    ));
                }
                if self.partition_table {
                    for tool in ["sfdisk", "losetup", "udevadm"] {
                        if !self.tools.get(tool).copied().unwrap_or(false) {
                            return Err(format!("Missing dependency: {tool}"));
                        }
                    }
                }
            } else {
                self.validate_import_path()?;
                if !self.tools.get("sfdisk").copied().unwrap_or(false) {
                    return Err("Missing dependency: sfdisk".into());
                }
                let path = expand_user_path(self.import_path.value())?;
                self.refresh_root_partition_choices(&path)?;
            }
        }
        Ok(())
    }
}
impl StepComponent for StorageStepView {
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        if let Some(index) = self.selected.selected() {
            context.storage.type_idx = index;
        }
        context.storage.creation_method_idx = self.creation_method.selected_idx();
        context.storage.disk_size = self.disk_size.value().into();
        if let Some((filesystem, _)) = self
            .fs_selection
            .selected()
            .and_then(|index| self.filesystems.get(index))
        {
            context.storage.disk_fs = *filesystem;
        }
        context.storage.disk_partition = self.partition_table;
        context.storage.import_path = expand_user_path(self.import_path.value())
            .unwrap_or_else(|_| self.import_path.value().trim().into())
            .to_string_lossy()
            .into_owned();
        context.storage.disk_root_partition = self.selected_root_partition();
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{
        DeploymentError, DeploymentRequest, HostHardwareSnapshot, ImagePartitionInfo,
        InterfaceValidation, ProvisioningHostSnapshot, ProvisioningPreparationPort,
    };
    use crate::tui::features::wizard::draft::StorageInfo;
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::path::Path;

    struct TestPreparation;

    #[async_trait]
    impl ProvisioningPreparationPort for TestPreparation {
        async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
            unreachable!("host inspection is not used by these tests")
        }

        async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
            unreachable!("hardware discovery is not used by these tests")
        }

        async fn validate_interface(
            &self,
            _name: &str,
            _bridge_mode: bool,
        ) -> Result<InterfaceValidation, DeploymentError> {
            unreachable!("interface validation is not used by these tests")
        }

        fn probe_image(
            &self,
            _path: &Path,
        ) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
            unreachable!("image probing is not used by these tests")
        }

        fn preview(&self, _request: &DeploymentRequest) -> String {
            unreachable!("preview generation is not used by these tests")
        }
    }

    fn view() -> StorageStepView {
        StorageStepView::new(
            &state(),
            Arc::new(ProvisioningPreparationService::new(Arc::new(
                TestPreparation,
            ))),
            BTreeMap::new(),
        )
    }

    fn state() -> StorageState {
        StorageState {
            type_idx: 0,
            info: StorageInfo {
                types: vec![(StorageBackendKind::DiskImage, true)],
                filesystems: vec![(DiskImageFilesystem::Ext4, true)],
            },
            creation_method_idx: 1,
            disk_size: "2G".into(),
            disk_fs: DiskImageFilesystem::Ext4,
            disk_partition: true,
            import_path: "/tmp/test.raw".into(),
            disk_root_partition: None,
        }
    }

    fn partition(number: u32, type_label: &str) -> ImagePartitionInfo {
        ImagePartitionInfo {
            number: DiskImagePartition::new(number).unwrap(),
            type_label: type_label.into(),
            current_architecture_root: false,
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
        let mut view = view();

        let error = view
            .apply_root_partition_probe(Some(probe.clone()))
            .unwrap_err();
        assert!(error.contains("select one from the list"));
        assert_eq!(view.root_partition_choices.len(), 3);

        view.root_selection.select(2);
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
        let mut view = view();
        let _ = view.apply_root_partition_probe(Some(probe));

        assert_eq!(view.root_partition_choices.len(), 13);
        assert_eq!(view.root_partition_list_height(), 8);
    }
}
