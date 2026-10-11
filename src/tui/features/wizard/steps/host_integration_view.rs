use crate::application::provisioning::HostGpuDevice;
use crate::domain::provisioning::{NetworkMode, PrivateUsersMode};
use crate::domain::x11::HostX11Socket;
use crate::tui::features::wizard::draft::{PassthroughConfig, WizardDraft, X11ProjectionSelection};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::widgets::display::content::ContentControl;
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
use std::collections::HashSet;

const PRIVILEGE_WARNING: &str = "Enabled setting: [Exec] Capability=all. This grants every Linux capability, including system and mount administration, device and raw I/O access, network administration, kernel/module controls, and process tracing. PrivateUsers and networking remain controlled by their separate settings. A compromised container may take over the host.";
const GPU_EMPTY: &str =
    "No compatible GPU devices found on the host. Graphics acceleration may not work as expected.";
const SCANNING: &str = " [~] Hardware discovery is running in the background. GPU and NVIDIA lists will populate automatically when finished...";
const X11_EMPTY: &str = "No verified filesystem X11 endpoints were found in the current desktop session. Host X11 integration can be configured later when a display becomes available.";
const X11_DIRECTORY: &str = "Expose /tmp/.X11-unix read-only. Future pathname X11 sockets in this directory remain available after desktop restarts; runtime ACL authorization still selects a concrete display.";
#[derive(Clone)]
enum GpuSelectionItem {
    AllDrm,
    Device(HostGpuDevice),
}

fn is_drm_gpu(gpu: &HostGpuDevice) -> bool {
    gpu.nodes.iter().any(|node| node.starts_with("/dev/dri/"))
}

fn toggle_gpu(items: &[GpuSelectionItem], checked: &mut HashSet<usize>, selected: usize) {
    let all_index = items
        .iter()
        .position(|item| matches!(item, GpuSelectionItem::AllDrm));
    if all_index == Some(selected) {
        let checking = checked.insert(selected);
        if !checking {
            checked.remove(&selected);
        }
        for (index, item) in items.iter().enumerate() {
            if matches!(item, GpuSelectionItem::Device(gpu) if is_drm_gpu(gpu)) {
                if checking {
                    checked.insert(index);
                } else {
                    checked.remove(&index);
                }
            }
        }
    } else {
        if !checked.insert(selected) {
            checked.remove(&selected);
        }
        if matches!(items.get(selected), Some(GpuSelectionItem::Device(gpu)) if is_drm_gpu(gpu)) {
            if let Some(all_index) = all_index {
                checked.remove(&all_index);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IntegrationControl {
    Scanning,
    Acceleration,
    Gpus,
    GpuEmpty,
    Privileged,
    PrivateUsers,
    X11Scope,
    X11Sockets,
    X11Empty,
    X11Directory,
    PrivilegeWarning,
}

pub struct HostIntegrationStepView {
    graphics_acceleration: bool,
    gpu_items: Vec<GpuSelectionItem>,
    gpu_selection: ListControlState,
    gpu_checked: HashSet<usize>,
    gpu_all_index: Option<usize>,
    privileged: bool,
    private_users: RadioState,
    form: FormState<IntegrationControl>,
    hardware_scanning: bool,
    private_network: bool,
    wayland_access_configured: bool,
    x11_directory_available: bool,
    x11_scope: RadioState,
    x11_sockets: Vec<HostX11Socket>,
    x11_selection: ListControlState,
    x11_checked: HashSet<usize>,
    preferred_x11_display: Option<u16>,
}
impl HostIntegrationStepView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial: &PassthroughConfig,
        network: Option<NetworkMode>,
        wayland_access_configured: bool,
        preferred_x11_display: Option<u16>,
        x11_sockets: Vec<HostX11Socket>,
        x11_directory_available: bool,
        gpus: Vec<HostGpuDevice>,
        hardware_scanning: bool,
    ) -> Self {
        let has_drm = gpus.iter().any(is_drm_gpu);
        let mut gpu_items = Vec::with_capacity(gpus.len() + usize::from(has_drm));
        if has_drm {
            gpu_items.push(GpuSelectionItem::AllDrm);
        }
        gpu_items.extend(gpus.into_iter().map(GpuSelectionItem::Device));
        let mut gpu_checked: HashSet<_> = gpu_items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                GpuSelectionItem::Device(gpu)
                    if gpu
                        .nodes
                        .iter()
                        .any(|node| initial.device_binds.contains(node)) =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect();
        let gpu_all_index = has_drm.then_some(0);
        if initial.gpu_passthrough_all {
            gpu_checked.extend(gpu_items.iter().enumerate().filter_map(|(index, item)| {
                matches!(item, GpuSelectionItem::AllDrm)
                    .then_some(index)
                    .or_else(|| {
                        matches!(item, GpuSelectionItem::Device(gpu) if is_drm_gpu(gpu))
                            .then_some(index)
                    })
            }));
        }
        let (x11_scope, x11_checked) = match &initial.x11_projection {
            X11ProjectionSelection::Directory => {
                (usize::from(x11_directory_available), HashSet::new())
            }
            X11ProjectionSelection::Sockets(selected) => (
                0,
                x11_sockets
                    .iter()
                    .enumerate()
                    .filter_map(|(index, socket)| {
                        selected
                            .iter()
                            .any(|selected| selected.source() == socket.source())
                            .then_some(index)
                    })
                    .collect(),
            ),
            X11ProjectionSelection::None => (0, HashSet::new()),
        };
        let private_users = match initial.private_users {
            Some(PrivateUsersMode::Pick) => 1,
            Some(PrivateUsersMode::Managed) => 2,
            Some(PrivateUsersMode::Yes) => 3,
            Some(PrivateUsersMode::No) => 4,
            _ => 0,
        };
        let mut view = Self {
            graphics_acceleration: initial.graphics_acceleration,
            gpu_items,
            gpu_selection: ListControlState::new(0),
            gpu_checked,
            gpu_all_index,
            privileged: initial.privileged,
            private_users: RadioState::new(private_users),
            form: FormState::new(IntegrationControl::Acceleration),
            hardware_scanning,
            private_network: network.as_ref().is_some_and(NetworkMode::is_private),
            wayland_access_configured,
            x11_directory_available,
            x11_scope: RadioState::new(x11_scope),
            x11_sockets,
            x11_selection: ListControlState::new(0),
            x11_checked,
            preferred_x11_display,
        };
        view.form.set_focus(true);
        view.with_form(80, |_| ());
        view
    }
    fn with_form<R>(
        &mut self,
        width: u16,
        visit: impl FnOnce(&mut Form<'_, IntegrationControl>) -> R,
    ) -> R {
        let is_accel = self.graphics_acceleration;
        let is_privileged = self.privileged;
        let has_gpus = !self.gpu_items.is_empty();
        let has_x11 = self.x11_directory_available || !self.x11_sockets.is_empty();
        let individual_x11 = self.x11_scope.selected_idx() == 0;
        let gpu_entries: Vec<_> = self
            .gpu_items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let label = match item {
                    GpuSelectionItem::AllDrm => "All DRM devices (/dev/dri)".into(),
                    GpuSelectionItem::Device(gpu) => format!(
                        "{} ({})",
                        gpu.display_name,
                        gpu.nodes.first().map(String::as_str).unwrap_or_default()
                    ),
                };
                ListEntry::new(label).checked(self.gpu_checked.contains(&index))
            })
            .collect();
        let x11_entries: Vec<_> = self
            .x11_sockets
            .iter()
            .enumerate()
            .map(|(index, socket)| {
                ListEntry::new(format!(
                    ":{}{}{}  {}",
                    socket.display(),
                    if socket.alternate() { "_" } else { "" },
                    if self.preferred_x11_display == Some(socket.display()) && !socket.alternate() {
                        "  (current DISPLAY)"
                    } else {
                        ""
                    },
                    socket.source().display()
                ))
                .checked(self.x11_checked.contains(&index))
            })
            .collect();
        let mut gpus =
            ListControl::new(&mut self.gpu_selection, "Select Host GPU(s)", &gpu_entries);
        let mut sockets = ListControl::new(
            &mut self.x11_selection,
            "Select Host X11 socket(s)",
            &x11_entries,
        );
        let mut acceleration = CheckboxControl::new(
            &mut self.graphics_acceleration,
            "Hardware Graphics Acceleration",
        );
        let mut privileged =
            CheckboxControl::new(&mut self.privileged, "Privileged Mode (NOT RECOMMENDED)");
        let mut users = RadioControl::new(
            &mut self.private_users,
            "PrivateUsers (User Namespace)",
            &["Default", "pick", "managed", "yes", "no"],
        );
        let scope_options: &[&str] = if self.x11_directory_available {
            &["Individual sockets", "Socket directory"]
        } else {
            &["Individual sockets"]
        };
        let mut scope = RadioControl::new(&mut self.x11_scope, "X11 projection", scope_options);
        let mut scanning = notice(" SCANNING ", SCANNING);
        let mut empty_gpus = notice(" No GPUs Detected ", GPU_EMPTY);
        let mut empty_x11 = notice(" Host X11 ", X11_EMPTY);
        let mut directory = notice(" Host X11 directory ", X11_DIRECTORY);
        let mut warning = notice("SECURITY RISK", PRIVILEGE_WARNING);
        let warning_height = notice_height(PRIVILEGE_WARNING, width);
        let directory_height = notice_height(X11_DIRECTORY, width);
        let gpu_height = (gpu_entries.len() as u16 + 2).min(10);
        let x11_height = (x11_entries.len() as u16 + 2).min(8);
        let mut items = Vec::new();
        if self.hardware_scanning {
            items.push(FormItem::field(IntegrationControl::Scanning, &mut scanning).height(4));
        }
        items.push(FormItem::field(
            IntegrationControl::Acceleration,
            &mut acceleration,
        ));
        if is_accel {
            if has_gpus {
                items.push(FormItem::field(IntegrationControl::Gpus, &mut gpus).height(gpu_height));
            } else {
                items.push(FormItem::field(
                    IntegrationControl::GpuEmpty,
                    &mut empty_gpus,
                ));
            }
        }
        items.extend([
            FormItem::field(IntegrationControl::Privileged, &mut privileged),
            FormItem::field(IntegrationControl::PrivateUsers, &mut users),
        ]);
        if has_x11 {
            items.push(FormItem::field(IntegrationControl::X11Scope, &mut scope));
            if individual_x11 && !x11_entries.is_empty() {
                items.push(
                    FormItem::field(IntegrationControl::X11Sockets, &mut sockets)
                        .height(x11_height),
                );
            } else if individual_x11 {
                items.push(FormItem::field(
                    IntegrationControl::X11Empty,
                    &mut empty_x11,
                ));
            } else {
                items.push(
                    FormItem::field(IntegrationControl::X11Directory, &mut directory)
                        .height(directory_height),
                );
            }
        } else {
            items.push(FormItem::field(
                IntegrationControl::X11Empty,
                &mut empty_x11,
            ));
        }
        if is_privileged {
            items.push(
                FormItem::field(IntegrationControl::PrivilegeWarning, &mut warning)
                    .height(warning_height),
            );
        }
        items.push(FormItem::spacer());
        visit(&mut Form::new(&mut self.form, items))
    }
    fn finish_input(&mut self, outcome: FormOutcome<IntegrationControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(IntegrationControl::Gpus) => {
                if let Some(index) = self.gpu_selection.selected() {
                    toggle_gpu(&self.gpu_items, &mut self.gpu_checked, index);
                }
                EventResult::Consumed
            }
            FormOutcome::Activated(IntegrationControl::X11Sockets) => {
                if let Some(index) = self.x11_selection.selected() {
                    if !self.x11_checked.insert(index) {
                        self.x11_checked.remove(&index);
                    }
                }
                EventResult::Consumed
            }
            _ => field_event(outcome),
        }
    }
    fn gpu_all_selected(&self) -> bool {
        self.gpu_all_index
            .is_some_and(|index| self.gpu_checked.contains(&index))
    }
    fn selected_gpu_nodes(&self) -> Vec<String> {
        if !self.graphics_acceleration {
            return Vec::new();
        }
        let mut nodes = Vec::new();
        for &index in &self.gpu_checked {
            if let Some(GpuSelectionItem::Device(gpu)) = self.gpu_items.get(index) {
                nodes.extend(
                    gpu.nodes
                        .iter()
                        .filter(|node| !self.gpu_all_selected() || !node.starts_with("/dev/dri/"))
                        .cloned(),
                );
            }
        }
        nodes.sort();
        nodes.dedup();
        nodes
    }
    fn selected_x11_sockets(&self) -> Vec<HostX11Socket> {
        self.x11_sockets
            .iter()
            .enumerate()
            .filter(|(index, _)| self.x11_checked.contains(index))
            .map(|(_, socket)| socket.clone())
            .collect()
    }
    fn selected_x11_projection(&self) -> X11ProjectionSelection {
        if self.x11_scope.selected_idx() == 1 && self.x11_directory_available {
            X11ProjectionSelection::Directory
        } else {
            let sockets = self.selected_x11_sockets();
            if sockets.is_empty() {
                X11ProjectionSelection::None
            } else {
                X11ProjectionSelection::Sockets(sockets)
            }
        }
    }
}
fn notice<'a>(label: &'a str, text: &'a str) -> ContentControl<Paragraph<'a>> {
    ContentControl::new(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(label)
                .border_style(Style::default().fg(theme::theme().border_unfocused)),
        ),
    )
}
fn notice_height(text: &str, width: u16) -> u16 {
    soft_wrap_text(text, usize::from(width.saturating_sub(2).max(1)))
        .len()
        .saturating_add(2)
        .min(usize::from(u16::MAX)) as u16
}
impl Component for HostIntegrationStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.inner(Margin::new(1, 1));
        self.with_form(area.width, |form| form.render(frame, area));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let outcome = self.with_form(self.form.viewport_width(), |form| form.handle_key(key));
        self.finish_input(outcome)
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(self.form.viewport_width(), |form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        if self.private_users.selected_idx() == 2 && !self.private_network {
            return Err("PrivateUsers=managed requires a private network mode".into());
        }
        if self.private_users.selected_idx() == 2 && self.wayland_access_configured {
            return Err("Wayland access is not supported with PrivateUsers=managed".into());
        }
        if self.private_users.selected_idx() == 2
            && !matches!(self.selected_x11_projection(), X11ProjectionSelection::None)
        {
            return Err("Host X11 projections are not supported with PrivateUsers=managed".into());
        }
        Ok(())
    }
}
impl StepComponent for HostIntegrationStepView {
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        context.passthrough.graphics_acceleration = self.graphics_acceleration;
        context.passthrough.gpu_passthrough_all =
            self.graphics_acceleration && self.gpu_all_selected();
        context.passthrough.privileged = self.privileged;
        context.passthrough.private_users = match self.private_users.selected_idx() {
            1 => Some(PrivateUsersMode::Pick),
            2 => Some(PrivateUsersMode::Managed),
            3 => Some(PrivateUsersMode::Yes),
            4 => Some(PrivateUsersMode::No),
            _ => None,
        };
        context.passthrough.selected_gpu_nodes = self.selected_gpu_nodes();
        context.passthrough.x11_projection = self.selected_x11_projection();
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::x11::X11SocketRevision;
    use crossterm::event::KeyCode;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn privileged_warning_names_the_effective_setting_and_independent_controls() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: true,
            private_users: None,
            graphics_acceleration: false,
            gpu_passthrough_all: false,
            nvidia_gpu: false,
            nvidia_profile: None,
            x11_projection: X11ProjectionSelection::None,
        };
        let mut view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            None,
            Vec::new(),
            false,
            Vec::new(),
            false,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("[Exec] Capability=all"));
        assert!(rendered.contains("PrivateUsers and networking remain controlled"));
    }

    #[test]
    fn all_drm_list_item_selects_drm_devices_and_keeps_individual_control() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: vec!["/dev/mali".into()],
            privileged: false,
            private_users: None,
            graphics_acceleration: true,
            gpu_passthrough_all: false,
            nvidia_gpu: false,
            nvidia_profile: None,
            x11_projection: X11ProjectionSelection::None,
        };
        let first_gpu = HostGpuDevice {
            display_name: "First DRM GPU".into(),
            driver_type: "DRM/KMS".into(),
            nodes: vec!["/dev/dri/card0".into(), "/dev/dri/renderD128".into()],
        };
        let second_gpu = HostGpuDevice {
            display_name: "Second DRM GPU".into(),
            driver_type: "DRM/KMS".into(),
            nodes: vec!["/dev/dri/card1".into(), "/dev/dri/renderD129".into()],
        };
        let legacy_gpu = HostGpuDevice {
            display_name: "Legacy Mali GPU".into(),
            driver_type: "Mali/Proprietary".into(),
            nodes: vec!["/dev/mali".into()],
        };
        let gpus = vec![first_gpu, second_gpu, legacy_gpu];
        let mut view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            None,
            Vec::new(),
            false,
            gpus.clone(),
            false,
        );

        assert!(!view.gpu_all_selected());
        assert_eq!(view.gpu_checked.len(), 1);
        view.form.select(IntegrationControl::Gpus);
        assert_eq!(view.gpu_selection.selected(), Some(0));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(view.gpu_all_selected());
        assert_eq!(view.gpu_checked.len(), 4);
        assert!(view.form.is_selected(IntegrationControl::Gpus));
        assert_eq!(view.selected_gpu_nodes(), vec!["/dev/mali"]);

        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        let mut pointer_view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            None,
            Vec::new(),
            false,
            gpus.clone(),
            false,
        );
        terminal
            .draw(|frame| pointer_view.render(frame, frame.area()))
            .unwrap();
        let gpu_area = pointer_view
            .form
            .control_area(IntegrationControl::Gpus)
            .unwrap();
        let click_gpu_row = |row| MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: gpu_area.x + 1,
            row: gpu_area.y + 1 + row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(
            pointer_view.handle_mouse(click_gpu_row(0)),
            EventResult::Consumed
        );
        assert_eq!(&pointer_view.gpu_checked, &view.gpu_checked);
        assert!(pointer_view.form.is_selected(IntegrationControl::Gpus));

        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("All DRM devices (/dev/dri)"));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert_eq!(view.gpu_selection.selected(), Some(1));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(!view.gpu_all_selected());
        assert!(!view.gpu_checked.contains(&1));
        assert!(view.gpu_checked.contains(&2));
        assert!(view.gpu_checked.contains(&3));
        assert_eq!(
            view.selected_gpu_nodes(),
            vec!["/dev/dri/card1", "/dev/dri/renderD129", "/dev/mali"]
        );
        pointer_view.handle_mouse(click_gpu_row(1));
        assert_eq!(&pointer_view.gpu_checked, &view.gpu_checked);
        assert_eq!(pointer_view.selected_gpu_nodes(), view.selected_gpu_nodes());

        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(!view.gpu_all_selected());
        assert_eq!(view.gpu_checked.len(), 3);

        let mut restored_config = config;
        restored_config.gpu_passthrough_all = true;
        let restored_view = HostIntegrationStepView::new(
            &restored_config,
            Some(NetworkMode::Host),
            false,
            None,
            Vec::new(),
            false,
            gpus,
            false,
        );
        assert!(restored_view.gpu_all_selected());
        assert_eq!(restored_view.gpu_checked.len(), 4);
        assert_eq!(restored_view.selected_gpu_nodes(), vec!["/dev/mali"]);
    }

    #[test]
    fn x11_selection_supports_multiple_endpoints_and_marks_current_display() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let socket = |name: &str, alternate: bool| {
            HostX11Socket::from_verified_parts(
                0,
                alternate,
                format!("/tmp/.X11-unix/{name}").into(),
                format!("/tmp/.X11-unix/{name}").into(),
                1000,
                1000,
                0o777,
                42,
                1000,
                1000,
                X11SocketRevision {
                    device: 1,
                    inode: if alternate { 2 } else { 1 },
                    ctime_seconds: 3,
                    ctime_nanoseconds: 4,
                },
            )
            .unwrap()
        };
        let standard = socket("X0", false);
        let alternate = socket("X0_", true);
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: false,
            private_users: None,
            graphics_acceleration: false,
            gpu_passthrough_all: false,
            nvidia_gpu: false,
            nvidia_profile: None,
            x11_projection: X11ProjectionSelection::None,
        };
        let mut view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            Some(0),
            vec![standard.clone(), alternate.clone()],
            true,
            Vec::new(),
            false,
        );

        assert!(view.selected_x11_sockets().is_empty());
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("(current DISPLAY)"));

        view.form.select(IntegrationControl::X11Sockets);
        assert_eq!(view.x11_selection.selected(), Some(0));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert_eq!(view.selected_x11_sockets(), vec![standard.clone()]);

        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert_eq!(view.x11_selection.selected(), Some(1));
        view.handle_key(KeyEvent::new(
            KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            view.selected_x11_sockets(),
            vec![standard.clone(), alternate.clone()]
        );

        let mut directory_config = config;
        directory_config.x11_projection = X11ProjectionSelection::Directory;
        let directory_view = HostIntegrationStepView::new(
            &directory_config,
            Some(NetworkMode::Host),
            false,
            Some(0),
            vec![standard.clone(), alternate.clone()],
            true,
            Vec::new(),
            false,
        );
        assert_eq!(
            directory_view.selected_x11_projection(),
            X11ProjectionSelection::Directory
        );
    }
}
