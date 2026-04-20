use crate::nspawn::adapters::storage::StorageType;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::path_box::PathBox;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{StorageState, WizardContext};
use crate::ui::wizard::steps::StepComponent;

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
            } else {
                // Import
                comps.push(&mut $self.import_path);
            }
        }
        comps
    }};
}

impl_wizard_nav!(StorageStepView, active_comps);

pub struct StorageStepView {
    list: SelectableList<(StorageType, bool)>,
    creation_method: RadioGroup,
    disk_size: TextBox,
    disk_fs: TextBox,
    import_path: PathBox,
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
                if v.trim().is_empty() {
                    Err("Size required".into())
                } else {
                    Ok(())
                }
            }),
            disk_fs: TextBox::new(
                " Filesystem Type (ext4, xfs) ",
                initial_data.disk_fs.clone(),
            )
            .with_validator(|v| {
                if v.trim().is_empty() {
                    Err("Filesystem required".into())
                } else {
                    Ok(())
                }
            }),
            import_path: PathBox::new(" Source Image Path ", initial_data.import_path.clone()),
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
            } else {
                constraints.push(Constraint::Length(3)); // Size
                constraints.push(Constraint::Length(3)); // FS
            }
        }
        constraints.push(Constraint::Length(1)); // Hint

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
            } else {
                self.disk_size.render(f, chunks[current]);
                current += 1;
                self.disk_fs.render(f, chunks[current]);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let res = delegate_wizard_navigation!(self, key, active_comps);

        if let EventResult::Consumed = res {
            if self.focus.active_idx == 0 {
                self.update_focus();
            }
        }
        res
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.list.set_focus(false);
            self.creation_method.set_focus(false);
            self.disk_size.set_focus(false);
            self.disk_fs.set_focus(false);
            self.import_path.set_focus(false);
        }
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
                self.disk_fs.validate()?;
            } else {
                self.import_path.validate()?;
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
        ctx.storage.disk_fs = self.disk_fs.value().to_string();
        ctx.storage.import_path = self.import_path.value().to_string();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
