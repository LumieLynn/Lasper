use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{SourceConfig, SourceKind, WizardContext};
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let cursor = $self.kind_list.selected_idx().unwrap_or(0);
        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.kind_list];
        match cursor {
            1 => comps.push(&mut $self.oci_url),
            2 => {
                comps.push(&mut $self.deboot_mirror);
                comps.push(&mut $self.deboot_suite);
                comps.push(&mut $self.bootstrap_pkgs);
            }
            3 => comps.push(&mut $self.bootstrap_pkgs),
            4 => {
                comps.push(&mut $self.pull_url);
                comps.push(&mut $self.pull_format);
            }
            5 => comps.push(&mut $self.local_path),
            _ => {}
        }
        comps
    }};
}

impl_wizard_nav!(SourceStepView, active_comps);

pub struct SourceStepView {
    kind_list: SelectableList<String>,
    oci_url: TextBox,
    deboot_mirror: TextBox,
    deboot_suite: TextBox,
    bootstrap_pkgs: TextBox,
    local_path: TextBox,
    pull_url: TextBox,
    pull_format: RadioGroup,
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
            "[pull]        download via pull-tar/pull-raw".to_string(),
            "[file]        import local filesystem/tarball (.tar, .raw)".to_string(),
        ];

        let kind_list = SelectableList::new(" Select base ", kinds, |s| s.clone());
        if initial_data.kind == SourceKind::Pull {
            // we'll set the index later or just let it be handled by context sync if needed
        }

        let mut view = Self {
            kind_list,
            oci_url: TextBox::new(" OCI URL (e.g. alpine, docker://ubuntu) ", initial_data.oci_url.clone())
                .with_validator(|v| {
                    let v = v.trim();
                    if v.is_empty() { return Err("URL required".into()); }
                    if v.contains("://") {
                        url::Url::parse(v).map(|_| ()).map_err(|e| format!("Invalid URL: {}", e))
                    } else {
                        // Allow basic OCI references: alphanumeric, ".", "-", "_", ":", "/"
                        if v.chars().all(|c| c.is_alphanumeric() || ".-_:".contains(c) || c == '/') {
                            Ok(())
                        } else {
                            Err("Invalid characters in image reference".into())
                        }
                    }
                }),
            deboot_mirror: TextBox::new(" Mirror (leave blank for default) ", initial_data.deboot_mirror.clone())
                .with_validator(|v| {
                    let v = v.trim();
                    if v.is_empty() { return Ok(()); }
                    url::Url::parse(v).map(|_| ()).map_err(|e| format!("Invalid URL: {}", e))
                }),
            deboot_suite: TextBox::new(" Suite (example: bookworm) ", initial_data.deboot_suite.clone())
                .with_validator(|v| if v.trim().is_empty() { Err("Suite required".into()) } else { Ok(()) }),
            bootstrap_pkgs: TextBox::new(" Packages (space separated) ", initial_data.bootstrap_pkgs.clone())
                .with_validator(|v| {
                    let v = v.trim();
                    if v.is_empty() { return Ok(()); }
                    if v.chars().all(|c| c.is_alphanumeric() || c.is_whitespace() || "+-.@_".contains(c)) {
                        Ok(())
                    } else {
                        Err("Invalid characters in package list".into())
                    }
                }),
            local_path: TextBox::new(" Local file path (.tar, .raw) ", initial_data.local_path.clone())
                .with_validator(|v| {
                    if v.trim().is_empty() { return Err("Path required".into()); }
                    let path = std::path::Path::new(v);
                    if !path.exists() { return Err("File not found".into()); }
                    let s = v.to_lowercase();
                    if s.ends_with(".tar") || s.ends_with(".tar.gz") || s.ends_with(".tar.xz") || s.ends_with(".tar.zst") || s.ends_with(".tgz") || s.ends_with(".raw") {
                        Ok(())
                    } else {
                        Err("Unsupported format (tar/raw only)".into())
                    }
                }),
            pull_url: TextBox::new(" Download URL (tar/raw) ", initial_data.pull_url.clone())
                .with_validator(|v| {
                    let v = v.trim();
                    if v.is_empty() {
                        Err("URL required".into())
                    } else {
                        url::Url::parse(v).map(|_| ()).map_err(|e| format!("Invalid URL: {}", e))
                    }
                }),
            pull_format: RadioGroup::new(
                " Pull Format ",
                vec!["Tarball (.tar)".to_string(), "Raw Image (.raw)".to_string()],
                if initial_data.is_pull_raw { 1 } else { 0 },
            ),
            oci_tip: TextBlock::new(" [!] OCI Important Note ", "OCI imports extract the rootfs only. You must manually configure the init program and entrypoint."),
            focus: FocusTracker::new(),
        };

        if initial_data.is_pull_raw {
            view.pull_format.set_selected_idx(1);
        }

        view.update_focus();
        view
    }

}

impl Component for SourceStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);

        let constraints = match cursor {
            1 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(self.oci_tip.required_height(area.width)),
            ], // OCI
            2 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ], // Deboot
            3 => vec![Constraint::Min(0), Constraint::Length(3)], // Pacstrap
            4 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
            ], // Pull
            5 => vec![Constraint::Min(0), Constraint::Length(3)], // LocalFile
            _ => vec![Constraint::Min(0)],                        // Copy/Clone
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
                self.bootstrap_pkgs.render(f, chunks[3]);
            }
            3 => self.bootstrap_pkgs.render(f, chunks[1]),
            4 => {
                self.pull_url.render(f, chunks[1]);
                self.pull_format.render(f, chunks[2]);
            }
            5 => self.local_path.render(f, chunks[1]),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let res = delegate_wizard_navigation!(self, key, active_comps);
        if let EventResult::Consumed = res {
            // If the kind list changed, we need to update focus since visible inputs change
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
            self.kind_list.set_focus(false);
            self.oci_url.set_focus(false);
            self.deboot_mirror.set_focus(false);
            self.deboot_suite.set_focus(false);
            self.bootstrap_pkgs.set_focus(false);
            self.local_path.set_focus(false);
            self.pull_url.set_focus(false);
            self.pull_format.set_focus(false);
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        let cursor = self.kind_list.selected_idx().unwrap_or(0);
        match cursor {
            1 => {
                crate::nspawn::ops::provision::builders::image::check_tool("skopeo")
                    .map_err(|_| "Missing dependency: skopeo".to_string())?;
                crate::nspawn::ops::provision::builders::image::check_tool("umoci")
                    .map_err(|_| "Missing dependency: umoci".to_string())?;
                self.oci_url.validate()?
            }
            2 => {
                crate::nspawn::ops::provision::builders::image::check_tool("debootstrap")
                    .map_err(|_| "Missing dependency: debootstrap".to_string())?;
                self.deboot_mirror.validate()?;
                self.deboot_suite.validate()?;
                self.bootstrap_pkgs.validate()?;
            }
            3 => {
                crate::nspawn::ops::provision::builders::image::check_tool("pacstrap")
                    .map_err(|_| "Missing dependency: pacstrap".to_string())?;
                self.bootstrap_pkgs.validate()?
            }
            4 => {
                crate::nspawn::ops::provision::builders::image::check_tool("curl")
                    .map_err(|_| "Missing dependency: curl".to_string())?;
                self.pull_url.validate()?
            }
            5 => self.local_path.validate()?,
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
            4 => SourceKind::Pull,
            _ => SourceKind::LocalFile,
        };
        ctx.source.oci_url = self.oci_url.value().to_string();
        ctx.source.deboot_mirror = self.deboot_mirror.value().to_string();
        ctx.source.deboot_suite = self.deboot_suite.value().to_string();
        ctx.source.bootstrap_pkgs = self.bootstrap_pkgs.value().to_string();
        ctx.source.local_path = self.local_path.value().to_string();
        ctx.source.pull_url = self.pull_url.value().to_string();
        ctx.source.is_pull_raw = self.pull_format.selected_idx() == 1;
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
