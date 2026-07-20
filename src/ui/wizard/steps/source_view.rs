use crate::nspawn::models::RootfsSourceSpec;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::inputs::path_box::expand_user_path;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::wizard::context::{ConfiguredSourceProfile, SourceKind, SourceState, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};

use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use unicode_width::UnicodeWidthStr;

macro_rules! active_comps {
    ($self:ident) => {{
        let selected_kind = $self.selected_kind();
        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.kind_list];
        match selected_kind {
            SourceKind::Oci => comps.push(&mut $self.oci_url),
            SourceKind::Debootstrap => {
                comps.push(&mut $self.deboot_mirror);
                comps.push(&mut $self.deboot_suite);
                comps.push(&mut $self.deboot_pkgs);
            }
            SourceKind::Pacstrap => comps.push(&mut $self.pacstrap_pkgs),
            SourceKind::Dnf5 => {
                comps.push(&mut $self.dnf_releasever);
                comps.push(&mut $self.dnf_pkgs);
            }
            SourceKind::Pull => {
                comps.push(&mut $self.pull_url);
                comps.push(&mut $self.pull_format);
            }
            SourceKind::LocalFile => comps.push(&mut $self.local_path),
            SourceKind::Copy | SourceKind::Profile { .. } => {}
        }
        comps
    }};
}

impl_wizard_nav!(SourceStepView, active_comps);

pub struct SourceStepView {
    kind_list: SelectableList<SourceKind>,
    oci_url: TextBox,
    deboot_mirror: TextBox,
    deboot_suite: TextBox,
    deboot_pkgs: TextBox,
    pacstrap_pkgs: TextBox,
    dnf_releasever: TextBox,
    dnf_pkgs: TextBox,
    local_path: TextBox,
    pull_url: TextBox,
    pull_format: crate::ui::widgets::selectors::radio_group::RadioGroup,
    profile_tip: TextBlock,
    profiles: Vec<ConfiguredSourceProfile>,
    focus: FocusTracker,
}

impl SourceStepView {
    pub fn new(initial_data: &SourceState) -> Self {
        let mut kinds = vec![
            SourceKind::Copy,
            SourceKind::Oci,
            SourceKind::Debootstrap,
            SourceKind::Pacstrap,
            SourceKind::Dnf5,
            SourceKind::Pull,
            SourceKind::LocalFile,
        ];
        kinds.extend(
            initial_data
                .profiles
                .iter()
                .map(|profile| SourceKind::Profile {
                    method: profile.method,
                    name: profile.name.clone(),
                }),
        );
        let selected_idx = kinds
            .iter()
            .position(|kind| kind == &initial_data.kind)
            .unwrap_or(0);
        let kind_column_width = source_kind_column_width(&kinds);
        let mut kind_list = SelectableList::new(" Select base ", kinds, move |kind| {
            source_kind_label(kind, kind_column_width)
        });
        kind_list.select(selected_idx);

        let mut view = Self {
            kind_list,
            oci_url: TextBox::new(
                " OCI URL (e.g. alpine, docker://ubuntu) ",
                initial_data.oci_url.clone(),
            )
            .with_validator(|v| {
                let v = v.trim();
                if v.is_empty() {
                    return Err("URL required".into());
                }
                if v.chars().any(|c| c.is_control() || "$`!\\\"'".contains(c)) {
                    return Err("Invalid characters in image reference".into());
                }
                if let Some(pos) = v.find("://") {
                    let prefix = &v[..pos];
                    if !matches!(prefix, "docker" | "docker-archive" | "docker-daemon") {
                        return Err(format!("Unknown transport prefix: {}://", prefix));
                    }
                    let remainder = &v[pos + 3..];
                    if remainder.is_empty() {
                        return Err("URL required".into());
                    }
                    if remainder
                        .chars()
                        .all(|c| c.is_alphanumeric() || ".-_:@".contains(c) || c == '/')
                    {
                        Ok(())
                    } else {
                        Err("Invalid characters in image reference".into())
                    }
                } else if v
                    .chars()
                    .all(|c| c.is_alphanumeric() || ".-_:@".contains(c) || c == '/')
                {
                    Ok(())
                } else {
                    Err("Invalid characters in image reference".into())
                }
            }),
            deboot_mirror: TextBox::new(
                " Mirror (leave blank for default) ",
                initial_data.deboot_mirror.clone(),
            )
            .with_validator(|v| {
                let v = v.trim();
                if v.is_empty() {
                    return Ok(());
                }
                url::Url::parse(v)
                    .map(|_| ())
                    .map_err(|e| format!("Invalid URL: {}", e))
            }),
            deboot_suite: TextBox::new(
                " Suite (example: bookworm) ",
                initial_data.deboot_suite.clone(),
            )
            .with_validator(|v| {
                if v.trim().is_empty() {
                    Err("Suite required".into())
                } else {
                    Ok(())
                }
            }),
            deboot_pkgs: TextBox::new(
                " Additional packages (space separated) ",
                initial_data.deboot_pkgs.clone(),
            )
            .with_validator(validate_package_text),
            pacstrap_pkgs: TextBox::new(
                " Additional packages (space separated) ",
                initial_data.pacstrap_pkgs.clone(),
            )
            .with_validator(validate_package_text),
            dnf_releasever: TextBox::new(
                " DNF5 releasever (example: 43) ",
                initial_data.dnf_releasever.clone(),
            )
            .with_validator(|v| {
                if v.trim().is_empty() {
                    Err("releasever required".into())
                } else {
                    Ok(())
                }
            }),
            dnf_pkgs: TextBox::new(
                " Additional packages (space separated) ",
                initial_data.dnf_pkgs.clone(),
            )
            .with_validator(validate_package_text),
            local_path: TextBox::new(
                " Local file path (.tar, .raw) ",
                initial_data.local_path.clone(),
            )
            .with_validator(|v| {
                if v.trim().is_empty() {
                    return Err("Path required".into());
                }
                let path = expand_user_path(v)?;
                if !path.is_file() {
                    return Err("File not found".into());
                }
                let s = path.to_string_lossy().to_lowercase();
                if is_tar_path(&s) || s.ends_with(".raw") || s.ends_with(".img") {
                    Ok(())
                } else {
                    Err("Unsupported format (tar/raw only)".into())
                }
            }),
            pull_url: TextBox::new(
                " Download URL (tar/raw) ",
                initial_data.pull_url.clone(),
            )
            .with_validator(|v| {
                let v = v.trim();
                if v.is_empty() {
                    Err("URL required".into())
                } else {
                    url::Url::parse(v)
                        .map(|_| ())
                        .map_err(|e| format!("Invalid URL: {}", e))
                }
            }),
            pull_format: crate::ui::widgets::selectors::radio_group::RadioGroup::new(
                " Pull Format ",
                vec!["Tarball (.tar)".to_string(), "Raw Image (.raw)".to_string()],
                if initial_data.is_pull_raw { 1 } else { 0 },
            ),
            profile_tip: TextBlock::new(
                " Configured profile ",
                "This source is defined in lasper.toml. Its provider policy is not editable in the wizard.",
            ),
            profiles: initial_data.profiles.clone(),
            focus: FocusTracker::new(),
        };

        view.update_focus();
        view
    }

    fn selected_kind(&self) -> SourceKind {
        self.kind_list
            .selected_item()
            .cloned()
            .unwrap_or(SourceKind::Copy)
    }

    fn selected_profile(
        &self,
        method: crate::nspawn::models::BootstrapMethod,
        name: &str,
    ) -> Option<&RootfsSourceSpec> {
        self.profiles
            .iter()
            .find(|profile| profile.method == method && profile.name == name)
            .map(|profile| &profile.source)
    }
}

impl Component for SourceStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let kind = self.selected_kind();
        let constraints = match kind {
            SourceKind::Oci => vec![Constraint::Min(0), Constraint::Length(3)],
            SourceKind::Debootstrap => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ],
            SourceKind::Pacstrap => vec![Constraint::Min(0), Constraint::Length(3)],
            SourceKind::Dnf5 => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
            ],
            SourceKind::Pull => vec![
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(3),
            ],
            SourceKind::LocalFile => vec![Constraint::Min(0), Constraint::Length(3)],
            SourceKind::Copy => vec![Constraint::Min(0)],
            SourceKind::Profile { .. } => vec![
                Constraint::Min(0),
                Constraint::Length(self.profile_tip.required_height(area.width)),
            ],
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        self.update_focus();
        self.kind_list.render(f, chunks[0]);
        match kind {
            SourceKind::Oci => self.oci_url.render(f, chunks[1]),
            SourceKind::Debootstrap => {
                self.deboot_mirror.render(f, chunks[1]);
                self.deboot_suite.render(f, chunks[2]);
                self.deboot_pkgs.render(f, chunks[3]);
            }
            SourceKind::Pacstrap => self.pacstrap_pkgs.render(f, chunks[1]),
            SourceKind::Dnf5 => {
                self.dnf_releasever.render(f, chunks[1]);
                self.dnf_pkgs.render(f, chunks[2]);
            }
            SourceKind::Pull => {
                self.pull_url.render(f, chunks[1]);
                self.pull_format.render(f, chunks[2]);
            }
            SourceKind::LocalFile => self.local_path.render(f, chunks[1]),
            SourceKind::Profile { .. } => self.profile_tip.render(f, chunks[1]),
            SourceKind::Copy => {}
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
        wizard_set_focus!(self, focused, active_comps);
    }

    fn validate(&mut self) -> Result<(), String> {
        match self.selected_kind() {
            SourceKind::Oci => {
                crate::nspawn::ops::provision::builders::image::check_tool("skopeo")
                    .map_err(|_| "Missing dependency: skopeo".to_string())?;
                crate::nspawn::ops::provision::builders::image::check_tool("umoci")
                    .map_err(|_| "Missing dependency: umoci".to_string())?;
                self.oci_url.validate()?;
            }
            SourceKind::Debootstrap => {
                check_tool("debootstrap")?;
                self.deboot_mirror.validate()?;
                self.deboot_suite.validate()?;
                self.deboot_pkgs.validate()?;
            }
            SourceKind::Pacstrap => {
                check_tool("pacstrap")?;
                self.pacstrap_pkgs.validate()?;
            }
            SourceKind::Dnf5 => {
                check_tool("dnf5")?;
                self.dnf_releasever.validate()?;
                self.dnf_pkgs.validate()?;
            }
            SourceKind::Pull => {
                check_tool("curl")?;
                self.pull_url.validate()?;
            }
            SourceKind::LocalFile => self.local_path.validate()?,
            SourceKind::Profile { method, name } => {
                let source = self
                    .selected_profile(method, &name)
                    .ok_or_else(|| "Configured profile is unavailable".to_string())?;
                if let Some(tool) = source.required_tool() {
                    check_tool(tool)?;
                }
                source.validate().map_err(|error| error.to_string())?;
                if let RootfsSourceSpec::Artifact(spec) = source {
                    let path = expand_user_path(&spec.path)?;
                    if !path.is_file() {
                        return Err("Configured artifact file not found".into());
                    }
                }
            }
            SourceKind::Copy => {}
        }
        Ok(())
    }
}

impl StepComponent for SourceStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.source.kind = self.selected_kind();
        ctx.source.oci_url = self.oci_url.value().to_string();
        ctx.source.deboot_mirror = self.deboot_mirror.value().to_string();
        ctx.source.deboot_suite = self.deboot_suite.value().to_string();
        ctx.source.deboot_pkgs = self.deboot_pkgs.value().to_string();
        ctx.source.pacstrap_pkgs = self.pacstrap_pkgs.value().to_string();
        ctx.source.dnf_releasever = self.dnf_releasever.value().to_string();
        ctx.source.dnf_pkgs = self.dnf_pkgs.value().to_string();
        ctx.source.local_path = expand_user_path(self.local_path.value())
            .unwrap_or_else(|_| self.local_path.value().trim().into())
            .to_string_lossy()
            .into_owned();
        ctx.source.pull_url = self.pull_url.value().to_string();
        ctx.source.is_pull_raw = self.pull_format.selected_idx() == 1;
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

fn source_kind_name(kind: &SourceKind) -> String {
    match kind {
        SourceKind::Copy => "copy / clone".into(),
        SourceKind::Oci => "OCI".into(),
        SourceKind::Debootstrap => "debootstrap / default".into(),
        SourceKind::Pacstrap => "pacstrap / default".into(),
        SourceKind::Dnf5 => "dnf5 / default".into(),
        SourceKind::Pull => "pull".into(),
        SourceKind::LocalFile => "artifact / default".into(),
        SourceKind::Profile { method, name } => {
            format!("{} / {}", bootstrap_method_label(*method), name)
        }
    }
}

fn source_kind_column_width(kinds: &[SourceKind]) -> usize {
    kinds
        .iter()
        .map(source_kind_name)
        .map(|name| UnicodeWidthStr::width(name.as_str()) + 2)
        .max()
        .unwrap_or(0)
}

fn source_kind_description(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Copy => "existing container",
        SourceKind::Oci => "registry rootfs import",
        SourceKind::Debootstrap => "Debian or Ubuntu",
        SourceKind::Pacstrap => "Arch Linux",
        SourceKind::Dnf5 => "Fedora or RPM",
        SourceKind::Pull => "network tar or raw image",
        SourceKind::LocalFile => "local tar or raw image",
        SourceKind::Profile { .. } => "configured profile",
    }
}

fn source_kind_label(kind: &SourceKind, kind_column_width: usize) -> String {
    let name = format!("[{}]", source_kind_name(kind));
    let padding = kind_column_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
    format!(
        "{name}{}  {}",
        " ".repeat(padding),
        source_kind_description(kind)
    )
}

fn bootstrap_method_label(method: crate::nspawn::models::BootstrapMethod) -> &'static str {
    match method {
        crate::nspawn::models::BootstrapMethod::Debootstrap => "debootstrap",
        crate::nspawn::models::BootstrapMethod::Pacstrap => "pacstrap",
        crate::nspawn::models::BootstrapMethod::Dnf5 => "dnf5",
        crate::nspawn::models::BootstrapMethod::Artifact => "artifact",
    }
}

fn check_tool(name: &str) -> Result<(), String> {
    crate::nspawn::ops::provision::builders::image::check_tool(name)
        .map_err(|_| format!("Missing dependency: {name}"))
}

fn validate_package_text(value: &str) -> Result<(), String> {
    if value
        .split_whitespace()
        .any(|package| package.starts_with('-'))
    {
        return Err("Package names cannot start with '-'".into());
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, ';' | '|' | '&' | '`' | '\'' | '"'))
    {
        Err("Invalid characters in package list".into())
    } else {
        Ok(())
    }
}

fn is_tar_path(path: &str) -> bool {
    path.ends_with(".tar")
        || path.ends_with(".tar.gz")
        || path.ends_with(".tar.xz")
        || path.ends_with(".tar.zst")
        || path.ends_with(".tgz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::BootstrapMethod;

    #[test]
    fn source_kind_descriptions_align_with_wide_profile_names() {
        let kinds = vec![
            SourceKind::Oci,
            SourceKind::Debootstrap,
            SourceKind::Profile {
                method: BootstrapMethod::Pacstrap,
                name: "中文配置".into(),
            },
        ];
        let width = source_kind_column_width(&kinds);

        for kind in kinds {
            let label = source_kind_label(&kind, width);
            let prefix = label
                .strip_suffix(source_kind_description(&kind))
                .expect("label ends with its description");
            assert_eq!(UnicodeWidthStr::width(prefix), width + 2);
        }
    }
}
