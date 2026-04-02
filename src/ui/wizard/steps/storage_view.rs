use crate::nspawn::storage::{StorageInfo, StorageType};
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crate::ui::wizard::context::{StorageConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::Paragraph,
    Frame,
};

pub struct StorageStepView {
    list: SelectableList<(StorageType, bool)>,
    raw_size: TextBox,
    raw_fs: TextBox,
    info: StorageInfo,
    focus: FocusTracker,
}

impl StorageStepView {
    pub fn new(initial_data: &StorageConfig, info: StorageInfo) -> Self {
        let types = info.types.clone();

        let mut list = SelectableList::new(" Storage Options ", types, |(st, supported)| {
            let status = if *supported { "" } else { " (unsupported)" };
            format!("{}{}", st.label(), status)
        });


        let type_idx = info
            .types
            .iter()
            .position(|(t, _)| *t == initial_data.storage_type)
            .unwrap_or(0);
        list.select(type_idx);

        let raw_cfg =
            initial_data
                .raw_config
                .clone()
                .unwrap_or(crate::nspawn::models::RawStorageConfig {
                    size: "2G".to_string(),
                    fs_type: "ext4".to_string(),
                    use_partition_table: false,
                });

        let mut view = Self {
            list,
            raw_size: TextBox::new(" Raw Image Size (e.g. 2G, 500M) ", raw_cfg.size)
                .with_validator(|v| {
                    if v.trim().is_empty() {
                        Err("Size required".into())
                    } else {
                        Ok(())
                    }
                }),

            raw_fs: TextBox::new(" Filesystem Type (ext4, xfs) ", raw_cfg.fs_type)
                .with_validator(|v| {
                    if v.trim().is_empty() {
                        Err("Filesystem required".into())
                    } else {
                        Ok(())
                    }
                }),

            info,
            focus: FocusTracker::new(),
        };
        view.update_focus();
        view
    }

    fn is_raw_selected(&self) -> bool {
        if let Some((st, _)) = self.list.selected_item() {
            return *st == StorageType::Raw;
        }
        false
    }

    fn next(&mut self) {
        let is_raw = self.is_raw_selected();
        let mut comps: Vec<&dyn Component> = vec![&self.list];
        if is_raw {
            comps.push(&self.raw_size);
            comps.push(&self.raw_fs);
        }
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let is_raw = self.is_raw_selected();
        let mut comps: Vec<&dyn Component> = vec![&self.list];
        if is_raw {
            comps.push(&self.raw_size);
            comps.push(&self.raw_fs);
        }
        self.focus.prev(&comps);
        self.update_focus();
    }

    fn update_focus(&mut self) {
        let is_raw = self.is_raw_selected();
        let mut comps: Vec<&mut dyn Component> = vec![&mut self.list];
        if is_raw {
            comps.push(&mut self.raw_size);
            comps.push(&mut self.raw_fs);
        }

        if self.focus.active_idx >= comps.len() {
            self.focus.active_idx = comps.len().saturating_sub(1);
        }
        self.focus.update_focus(&mut comps, true);
    }
}

impl Component for StorageStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let is_raw = self.is_raw_selected();

        let mut constraints = vec![
            Constraint::Length(1), // Title
            Constraint::Min(0),    // List
        ];
        if is_raw {
            constraints.push(Constraint::Length(3)); // Size
            constraints.push(Constraint::Length(3)); // FS
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

        if is_raw {
            self.raw_size.render(f, chunks[2]);
            self.raw_fs.render(f, chunks[3]);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Tab => {
                self.next();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                self.prev();
                return EventResult::Consumed;
            }
            _ => {}
        }

        let is_raw = self.is_raw_selected();
        let mut comps: Vec<&mut dyn Component> = vec![&mut self.list];
        if is_raw {
            comps.push(&mut self.raw_size);
            comps.push(&mut self.raw_fs);
        }

        if self.focus.active_idx >= comps.len() {
            self.focus.active_idx = comps.len().saturating_sub(1);
        }

        let res = comps[self.focus.active_idx].handle_key(key);

        if let EventResult::Consumed = res {
            if self.focus.active_idx == 0 {
                self.update_focus();
            }
        }
        match res {

            EventResult::FocusNext => {
                self.next();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                self.prev();
                EventResult::Consumed
            }
            _ => res,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.list.set_focus(false);
            self.raw_size.set_focus(false);
            self.raw_fs.set_focus(false);
        }
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.is_raw_selected() {
            self.raw_size.validate()?;
            self.raw_fs.validate()?;
        }
        Ok(())
    }
}

impl StepComponent for StorageStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        if let Some(idx) = self.list.selected_idx() {
            ctx.storage.type_idx = idx;
        }
        ctx.storage.raw_size = self.raw_size.value().to_string();
        ctx.storage.raw_fs = self.raw_fs.value().to_string();
    }
}
