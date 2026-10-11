use crate::application::provisioning::HostCapability;
use crate::domain::bootstrap::RootfsSourceSpec;
use crate::domain::oci::OciReference;
use crate::domain::provisioning::OciNetworkMode;
use crate::domain::source::BootstrapMethod;
use crate::tui::features::wizard::core::input::expand_user_path;
use crate::tui::features::wizard::draft::{
    ConfiguredSourceProfile, SourceKind, SourceState, WizardDraft,
};
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
use crate::tui::foundation::{text::soft_wrap_text, theme};
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};
use std::collections::{BTreeMap, HashMap};
use unicode_width::UnicodeWidthStr;

const PROFILE_TIP: &str =
    "This source is defined in lasper.toml. Its provider policy is not editable in the wizard.";
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SourceControl {
    Kind,
    OciUrl,
    OciMode,
    OciNetwork,
    DebootMirror,
    DebootSuite,
    DebootDefaults,
    DebootPackages,
    PacstrapDefaults,
    PacstrapPackages,
    DnfRelease,
    DnfDefaults,
    DnfPackages,
    LocalPath,
    PullUrl,
    PullFormat,
    ProfileTip,
}

pub struct SourceStepView {
    kinds: Vec<SourceKind>,
    selected: ListControlState,
    oci_url: Input,
    oci_mode: RadioState,
    oci_network: RadioState,
    deboot_mirror: Input,
    deboot_suite: Input,
    deboot_inherit_default_packages: bool,
    deboot_pkgs: Input,
    pacstrap_inherit_default_packages: bool,
    pacstrap_pkgs: Input,
    dnf_releasever: Input,
    dnf_inherit_default_packages: bool,
    dnf_pkgs: Input,
    local_path: Input,
    pull_url: Input,
    pull_format: RadioState,
    errors: HashMap<SourceControl, String>,
    profiles: Vec<ConfiguredSourceProfile>,
    oci: HostCapability,
    tools: BTreeMap<String, bool>,
    form: FormState<SourceControl>,
}
impl SourceStepView {
    pub fn new(initial: &SourceState, oci: HostCapability, tools: BTreeMap<String, bool>) -> Self {
        let mut kinds = vec![
            SourceKind::Copy,
            SourceKind::Oci,
            SourceKind::Debootstrap,
            SourceKind::Pacstrap,
            SourceKind::Dnf5,
            SourceKind::Pull,
            SourceKind::LocalFile,
        ];
        kinds.extend(initial.profiles.iter().map(|profile| SourceKind::Profile {
            method: profile.method,
            name: profile.name.clone(),
        }));
        let selected = kinds
            .iter()
            .position(|kind| kind == &initial.kind)
            .unwrap_or(0);
        let mut view = Self {
            kinds,
            selected: ListControlState::new(selected),
            oci_url: Input::from(initial.oci_url.clone()),
            oci_mode: RadioState::new(usize::from(initial.oci_read_only)),
            oci_network: RadioState::new(match initial.oci_network {
                OciNetworkMode::Host => 0,
                OciNetworkMode::Isolated => 1,
                OciNetworkMode::Veth => 2,
            }),
            deboot_mirror: Input::from(initial.deboot_mirror.clone()),
            deboot_suite: Input::from(initial.deboot_suite.clone()),
            deboot_inherit_default_packages: initial.deboot_inherit_default_packages,
            deboot_pkgs: Input::from(initial.deboot_pkgs.clone()),
            pacstrap_inherit_default_packages: initial.pacstrap_inherit_default_packages,
            pacstrap_pkgs: Input::from(initial.pacstrap_pkgs.clone()),
            dnf_releasever: Input::from(initial.dnf_releasever.clone()),
            dnf_inherit_default_packages: initial.dnf_inherit_default_packages,
            dnf_pkgs: Input::from(initial.dnf_pkgs.clone()),
            local_path: Input::from(initial.local_path.clone()),
            pull_url: Input::from(initial.pull_url.clone()),
            pull_format: RadioState::new(usize::from(initial.is_pull_raw)),
            errors: HashMap::new(),
            profiles: initial.profiles.clone(),
            oci,
            tools,
            form: FormState::new(SourceControl::Kind),
        };
        view.form.set_focus(true);
        view.with_form(80, |_| ());
        view
    }
    fn selected_kind(&self) -> SourceKind {
        self.selected
            .selected()
            .and_then(|index| self.kinds.get(index))
            .cloned()
            .unwrap_or(SourceKind::Copy)
    }
    fn selected_profile(&self, method: BootstrapMethod, name: &str) -> Option<&RootfsSourceSpec> {
        self.profiles
            .iter()
            .find(|profile| profile.method == method && profile.name == name)
            .map(|profile| &profile.source)
    }
    fn record_error(
        &mut self,
        id: SourceControl,
        result: Result<(), String>,
    ) -> Result<(), String> {
        if let Err(error) = &result {
            self.errors.insert(id, error.clone());
        } else {
            self.errors.remove(&id);
        }
        result
    }
    fn with_form<R>(
        &mut self,
        width: u16,
        visit: impl FnOnce(&mut Form<'_, SourceControl>) -> R,
    ) -> R {
        let kind = self.selected_kind();
        let column_width = source_kind_column_width(&self.kinds);
        let entries: Vec<_> = self
            .kinds
            .iter()
            .map(|kind| ListEntry::new(source_kind_label(kind, column_width)))
            .collect();
        let mut kinds = ListControl::new(&mut self.selected, " Select base ", &entries);
        let mut oci_url = TextInput::new(
            &mut self.oci_url,
            " OCI registry reference (e.g. docker.io/library/nginx:latest) ",
            self.errors.get(&SourceControl::OciUrl).map(String::as_str),
        );
        let mut oci_mode = RadioControl::new(
            &mut self.oci_mode,
            " OCI Storage ",
            &["Writable overlay", "Read-only layers"],
        );
        let mut oci_network = RadioControl::new(
            &mut self.oci_network,
            " OCI Network ",
            &["Host", "Isolated", "Veth"],
        );
        let mut deboot_mirror = TextInput::new(
            &mut self.deboot_mirror,
            " Mirror (leave blank for default) ",
            self.errors
                .get(&SourceControl::DebootMirror)
                .map(String::as_str),
        );
        let mut deboot_suite = TextInput::new(
            &mut self.deboot_suite,
            " Suite (example: bookworm) ",
            self.errors
                .get(&SourceControl::DebootSuite)
                .map(String::as_str),
        );
        let mut deboot_defaults = CheckboxControl::new(
            &mut self.deboot_inherit_default_packages,
            "Include default packages",
        );
        let mut deboot_packages = TextInput::new(
            &mut self.deboot_pkgs,
            " Packages (space separated) ",
            self.errors
                .get(&SourceControl::DebootPackages)
                .map(String::as_str),
        );
        let mut pacstrap_defaults = CheckboxControl::new(
            &mut self.pacstrap_inherit_default_packages,
            "Include default packages",
        );
        let mut pacstrap_packages = TextInput::new(
            &mut self.pacstrap_pkgs,
            " Packages (space separated) ",
            self.errors
                .get(&SourceControl::PacstrapPackages)
                .map(String::as_str),
        );
        let mut dnf_release = TextInput::new(
            &mut self.dnf_releasever,
            " DNF5 releasever (example: 43) ",
            self.errors
                .get(&SourceControl::DnfRelease)
                .map(String::as_str),
        );
        let mut dnf_defaults = CheckboxControl::new(
            &mut self.dnf_inherit_default_packages,
            "Include default packages",
        );
        let mut dnf_packages = TextInput::new(
            &mut self.dnf_pkgs,
            " Packages (space separated) ",
            self.errors
                .get(&SourceControl::DnfPackages)
                .map(String::as_str),
        );
        let mut local_path = TextInput::new(
            &mut self.local_path,
            " Local file path (.tar, .raw) ",
            self.errors
                .get(&SourceControl::LocalPath)
                .map(String::as_str),
        );
        let mut pull_url = TextInput::new(
            &mut self.pull_url,
            " Download URL (tar/raw) ",
            self.errors.get(&SourceControl::PullUrl).map(String::as_str),
        );
        let mut pull_format = RadioControl::new(
            &mut self.pull_format,
            " Pull Format ",
            &["Tarball (.tar)", "Raw Image (.raw)"],
        );
        let tip_height = soft_wrap_text(PROFILE_TIP, usize::from(width.saturating_sub(2).max(1)))
            .len()
            .saturating_add(2)
            .min(usize::from(u16::MAX)) as u16;
        let mut tip = ContentControl::new(
            Paragraph::new(PROFILE_TIP)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" Configured profile ")
                        .border_style(Style::default().fg(theme::theme().border_unfocused)),
                ),
        );
        let mut items = vec![FormItem::field(SourceControl::Kind, &mut kinds).fill(4)];
        match kind {
            SourceKind::Oci => items.extend([
                FormItem::field(SourceControl::OciUrl, &mut oci_url),
                FormItem::field(SourceControl::OciMode, &mut oci_mode),
                FormItem::field(SourceControl::OciNetwork, &mut oci_network),
            ]),
            SourceKind::Debootstrap => items.extend([
                FormItem::field(SourceControl::DebootMirror, &mut deboot_mirror),
                FormItem::field(SourceControl::DebootSuite, &mut deboot_suite),
                FormItem::field(SourceControl::DebootDefaults, &mut deboot_defaults),
                FormItem::field(SourceControl::DebootPackages, &mut deboot_packages),
            ]),
            SourceKind::Pacstrap => items.extend([
                FormItem::field(SourceControl::PacstrapDefaults, &mut pacstrap_defaults),
                FormItem::field(SourceControl::PacstrapPackages, &mut pacstrap_packages),
            ]),
            SourceKind::Dnf5 => items.extend([
                FormItem::field(SourceControl::DnfRelease, &mut dnf_release),
                FormItem::field(SourceControl::DnfDefaults, &mut dnf_defaults),
                FormItem::field(SourceControl::DnfPackages, &mut dnf_packages),
            ]),
            SourceKind::Pull => items.extend([
                FormItem::field(SourceControl::PullUrl, &mut pull_url),
                FormItem::field(SourceControl::PullFormat, &mut pull_format),
            ]),
            SourceKind::LocalFile => {
                items.push(FormItem::field(SourceControl::LocalPath, &mut local_path))
            }
            SourceKind::Profile { .. } => {
                items.push(FormItem::field(SourceControl::ProfileTip, &mut tip).height(tip_height))
            }
            SourceKind::Copy => {}
        }
        visit(&mut Form::new(&mut self.form, items))
    }
}
impl Component for SourceStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.inner(Margin::new(1, 1));
        self.with_form(area.width, |form| form.render(frame, area));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        field_event(self.with_form(self.form.viewport_width(), |form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        field_event(self.with_form(self.form.viewport_width(), |form| form.handle_mouse(mouse)))
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        match self.selected_kind() {
            SourceKind::Oci => {
                if !self.oci.available {
                    return Err(self
                        .oci
                        .reason
                        .clone()
                        .unwrap_or_else(|| "OCI import is unavailable on this host".into()));
                }
                self.record_error(
                    SourceControl::OciUrl,
                    OciReference::new(self.oci_url.value().trim())
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                )?;
            }
            SourceKind::Debootstrap => {
                check_bootstrap_tools(&self.tools, BootstrapMethod::Debootstrap)?;
                self.record_error(
                    SourceControl::DebootMirror,
                    validate_url(self.deboot_mirror.value(), true),
                )?;
                self.record_error(
                    SourceControl::DebootSuite,
                    required_text(self.deboot_suite.value(), "Suite required"),
                )?;
                self.record_error(
                    SourceControl::DebootPackages,
                    validate_package_text(self.deboot_pkgs.value()),
                )?;
            }
            SourceKind::Pacstrap => {
                check_bootstrap_tools(&self.tools, BootstrapMethod::Pacstrap)?;
                self.record_error(
                    SourceControl::PacstrapPackages,
                    validate_package_text(self.pacstrap_pkgs.value()),
                )?;
            }
            SourceKind::Dnf5 => {
                check_bootstrap_tools(&self.tools, BootstrapMethod::Dnf5)?;
                self.record_error(
                    SourceControl::DnfRelease,
                    required_text(self.dnf_releasever.value(), "releasever required"),
                )?;
                self.record_error(
                    SourceControl::DnfPackages,
                    validate_package_text(self.dnf_pkgs.value()),
                )?;
            }
            SourceKind::Pull => {
                check_tool(&self.tools, "curl")?;
                self.record_error(
                    SourceControl::PullUrl,
                    validate_url(self.pull_url.value(), false),
                )?;
            }
            SourceKind::LocalFile => {
                self.record_error(
                    SourceControl::LocalPath,
                    validate_local_path(self.local_path.value()),
                )?;
            }
            SourceKind::Profile { method, name } => {
                let source = self
                    .selected_profile(method, &name)
                    .ok_or_else(|| "Configured profile is unavailable".to_string())?;
                check_bootstrap_tools(&self.tools, source.method())?;
                source.validate().map_err(|error| error.to_string())?;
                if let RootfsSourceSpec::Artifact(spec) = source {
                    if !expand_user_path(&spec.path)?.is_file() {
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
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        context.source.kind = self.selected_kind();
        context.source.oci_url = self.oci_url.value().to_string();
        context.source.oci_read_only = self.oci_mode.selected_idx() == 1;
        context.source.oci_network = match self.oci_network.selected_idx() {
            1 => OciNetworkMode::Isolated,
            2 => OciNetworkMode::Veth,
            _ => OciNetworkMode::Host,
        };
        context.source.deboot_mirror = self.deboot_mirror.value().to_string();
        context.source.deboot_suite = self.deboot_suite.value().to_string();
        context.source.deboot_inherit_default_packages = self.deboot_inherit_default_packages;
        context.source.deboot_pkgs = self.deboot_pkgs.value().to_string();
        context.source.pacstrap_inherit_default_packages = self.pacstrap_inherit_default_packages;
        context.source.pacstrap_pkgs = self.pacstrap_pkgs.value().to_string();
        context.source.dnf_releasever = self.dnf_releasever.value().to_string();
        context.source.dnf_inherit_default_packages = self.dnf_inherit_default_packages;
        context.source.dnf_pkgs = self.dnf_pkgs.value().to_string();
        context.source.local_path = expand_user_path(self.local_path.value())
            .unwrap_or_else(|_| self.local_path.value().trim().into())
            .to_string_lossy()
            .into_owned();
        context.source.pull_url = self.pull_url.value().to_string();
        context.source.is_pull_raw = self.pull_format.selected_idx() == 1;
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}
fn required_text(value: &str, message: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(message.into())
    } else {
        Ok(())
    }
}
fn validate_url(value: &str, optional: bool) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return if optional {
            Ok(())
        } else {
            Err("URL required".into())
        };
    }
    url::Url::parse(value)
        .map(|_| ())
        .map_err(|error| format!("Invalid URL: {error}"))
}
fn validate_local_path(value: &str) -> Result<(), String> {
    required_text(value, "Path required")?;
    let path = expand_user_path(value)?;
    if !path.is_file() {
        return Err("File not found".into());
    }
    let path = path.to_string_lossy().to_lowercase();
    if is_tar_path(&path) || path.ends_with(".raw") || path.ends_with(".img") {
        Ok(())
    } else {
        Err("Unsupported format (tar/raw only)".into())
    }
}

fn source_kind_name(kind: &SourceKind) -> String {
    match kind {
        SourceKind::Copy => "copy / clone".into(),
        SourceKind::Oci => "OCI application".into(),
        SourceKind::Debootstrap => "debootstrap / default".into(),
        SourceKind::Pacstrap => "pacstrap / default".into(),
        SourceKind::Dnf5 => "dnf5 / default".into(),
        SourceKind::Pull => "pull".into(),
        SourceKind::LocalFile => "artifact / default".into(),
        SourceKind::Profile { method, name } => format!("{method} / {name}"),
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
        SourceKind::Copy => "existing image",
        SourceKind::Oci => "systemd 260+ experimental",
        SourceKind::Debootstrap => "Debian or Ubuntu",
        SourceKind::Pacstrap => "Arch Linux",
        SourceKind::Dnf5 => "Fedora / DNF5",
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

fn check_tool(tools: &BTreeMap<String, bool>, name: &str) -> Result<(), String> {
    if tools.get(name).copied().unwrap_or(false) {
        Ok(())
    } else {
        Err(format!("Missing dependency: {name}"))
    }
}

fn check_bootstrap_tools(
    tools: &BTreeMap<String, bool>,
    method: BootstrapMethod,
) -> Result<(), String> {
    method
        .required_tools()
        .iter()
        .try_for_each(|tool| check_tool(tools, tool))
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
    use crate::domain::bootstrap::PacstrapSpec;
    use crate::domain::source::BootstrapMethod;

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

    #[test]
    fn pacstrap_forms_and_profiles_require_both_host_tools() {
        theme::init_theme(theme::Theme::dark());
        let mut draft = WizardDraft::new(
            Vec::new(),
            Vec::new(),
            Default::default(),
            Default::default(),
            crate::config::AppConfig::default().bootstrap.resolve(),
        );
        draft.source.profiles.push(ConfiguredSourceProfile {
            method: BootstrapMethod::Pacstrap,
            name: "custom".into(),
            source: RootfsSourceSpec::Pacstrap(PacstrapSpec::default()),
        });

        for kind in [
            SourceKind::Pacstrap,
            SourceKind::Profile {
                method: BootstrapMethod::Pacstrap,
                name: "custom".into(),
            },
        ] {
            draft.source.kind = kind;
            for (pacstrap, pacman, missing) in [
                (true, false, Some("pacman")),
                (false, true, Some("pacstrap")),
                (true, true, None),
            ] {
                let tools =
                    BTreeMap::from([("pacstrap".into(), pacstrap), ("pacman".into(), pacman)]);
                let mut view =
                    SourceStepView::new(&draft.source, HostCapability::available(), tools);
                let expected =
                    missing.map_or(Ok(()), |tool| Err(format!("Missing dependency: {tool}")));
                assert_eq!(view.validate(), expected);
                if !pacman {
                    view.tools.remove("pacman");
                    assert_eq!(view.validate(), Err("Missing dependency: pacman".into()));
                }
            }
        }
    }
}
