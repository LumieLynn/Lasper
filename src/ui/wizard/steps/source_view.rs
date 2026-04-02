use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crate::ui::wizard::context::{SourceConfig, SourceKind, WizardContext};
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub struct SourceStepView {
    kind_list: SelectableList<String>,
    oci_url: TextBox,
    deboot_mirror: TextBox,
    deboot_suite: TextBox,
    pacstrap_pkgs: TextBox,
    disk_path: TextBox,
    oci_tip: TextBlock,

    focus: FocusTracker,
}

impl SourceStepView {
    pub fn new(initial_data: &SourceConfig) -> Self {
        let kinds = vec![
            "[copy/clone]  copy an existing container".to_string(),
            "[OCI]         import from registry (docker.io/…)".to_string(),
            "[debootstrap] bootstrap Debian/Ubuntu container".to_string(),
            "[pacstrap]    bootstrap Arch Linux container".to_string(),
            "[disk]        import local disk image (.raw, .tar)".to_string(),
        ];

        let kind_list = SelectableList::new(" Select base ", kinds, |s| s.clone());

        let mut view = Self {
            kind_list,
            oci_url: TextBox::new(" OCI URL (e.g. alpine, docker://ubuntu) ", initial_data.oci_url.clone())
                .with_validator(|v| if v.trim().is_empty() { Err("URL required".into()) } else { Ok(()) }),
            deboot_mirror: TextBox::new(" Mirror (leave blank for default) ", initial_data.deboot_mirror.clone()),
            deboot_suite: TextBox::new(" Suite (example: bookworm) ", initial_data.deboot_suite.clone())
                .with_validator(|v| if v.trim().is_empty() { Err("Suite required".into()) } else { Ok(()) }),
            pacstrap_pkgs: TextBox::new(" Packages (space separated) ", initial_data.pacstrap_pkgs.clone()),
            disk_path: TextBox::new(" Local file path (.raw, .tar) ", initial_data.disk_path.clone())
                .with_validator(|v| if v.trim().is_empty() { Err("Path required".into()) } else { Ok(()) }),
            oci_tip: TextBlock::new(" [!] OCI Important Note ", "OCI imports extract the rootfs only. You must manually configure the init program and entrypoint."),
            focus: FocusTracker::new(),
        };

        view.update_focus();
        view
    }

    fn next(&mut self) {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        let mut comps: Vec<&dyn Component> = vec![&self.kind_list];
        match cursor {
            1 => comps.push(&self.oci_url),
            2 => {
                comps.push(&self.deboot_mirror);
                comps.push(&self.deboot_suite);
            }
            3 => comps.push(&self.pacstrap_pkgs),
            4 => comps.push(&self.disk_path),
            _ => {}
        }
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        let mut comps: Vec<&dyn Component> = vec![&self.kind_list];
        match cursor {
            1 => comps.push(&self.oci_url),
            2 => {
                comps.push(&self.deboot_mirror);
                comps.push(&self.deboot_suite);
            }
            3 => comps.push(&self.pacstrap_pkgs),
            4 => comps.push(&self.disk_path),
            _ => {}
        }
        self.focus.prev(&comps);
        self.update_focus();
    }

    fn update_focus(&mut self) {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        let mut comps: Vec<&mut dyn Component> = vec![&mut self.kind_list];
        match cursor {
            1 => comps.push(&mut self.oci_url),
            2 => {
                comps.push(&mut self.deboot_mirror);
                comps.push(&mut self.deboot_suite);
            }
            3 => comps.push(&mut self.pacstrap_pkgs),
            4 => comps.push(&mut self.disk_path),
            _ => {}
        }
        // clamp focus internally inside update_focus to prevent out of bounds when selection changes
        if self.focus.active_idx >= comps.len() {
            self.focus.active_idx = comps.len().saturating_sub(1);
        }
        self.focus.update_focus(&mut comps, true);
    }
}

impl Component for SourceStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);

        let constraints = match cursor {
            1 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(4),
            ], // OCI
            2 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
            ], // Deboot
            3 | 4 => vec![Constraint::Min(0), Constraint::Length(3)], // Pacstrap/Disk
            _ => vec![Constraint::Min(0)],                            // Copy/Clone
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        self.update_focus();

        self.kind_list.render(f, chunks[0]);

        match cursor {
            1 => {
                self.oci_url.render(f, chunks[1]);
                self.oci_tip.render(f, chunks[2]);
            }
            2 => {
                self.deboot_mirror.render(f, chunks[1]);
                self.deboot_suite.render(f, chunks[2]);
            }
            3 => self.pacstrap_pkgs.render(f, chunks[1]),
            4 => self.disk_path.render(f, chunks[1]),
            _ => {}
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

        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        let mut comps: Vec<&mut dyn Component> = vec![&mut self.kind_list];
        match cursor {
            1 => comps.push(&mut self.oci_url),
            2 => {
                comps.push(&mut self.deboot_mirror);
                comps.push(&mut self.deboot_suite);
            }
            3 => comps.push(&mut self.pacstrap_pkgs),
            4 => comps.push(&mut self.disk_path),
            _ => {}
        }

        if self.focus.active_idx >= comps.len() {
            self.focus.active_idx = comps.len().saturating_sub(1);
        }

        let res = comps[self.focus.active_idx].handle_key(key);
        if let EventResult::Consumed = res {
            // If the kind list changed, we need to update focus since visible inputs change
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
            self.kind_list.set_focus(false);
            self.oci_url.set_focus(false);
            self.deboot_mirror.set_focus(false);
            self.deboot_suite.set_focus(false);
            self.pacstrap_pkgs.set_focus(false);
            self.disk_path.set_focus(false);
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        match cursor {
            1 => self.oci_url.validate()?,
            2 => {
                self.deboot_mirror.validate()?;
                self.deboot_suite.validate()?;
            }
            3 => self.pacstrap_pkgs.validate()?,
            4 => self.disk_path.validate()?,
            _ => {}
        }
        Ok(())
    }
}

impl StepComponent for SourceStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        let idx = self.kind_list.selected_idx().unwrap_or(0);
        ctx.source.kind = match idx {
            0 => SourceKind::Copy,
            1 => SourceKind::Oci,
            2 => SourceKind::Debootstrap,
            3 => SourceKind::Pacstrap,
            _ => SourceKind::DiskImage,
        };
        ctx.source.oci_url = self.oci_url.value().to_string();
        ctx.source.deboot_mirror = self.deboot_mirror.value().to_string();
        ctx.source.deboot_suite = self.deboot_suite.value().to_string();
        ctx.source.pacstrap_pkgs = self.pacstrap_pkgs.value().to_string();
        ctx.source.disk_path = self.disk_path.value().to_string();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
