use crate::nspawn::models::{NetworkMode, PrivateUsersMode};
use crate::nspawn::platform::gpu::GpuDevice;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::lists::checklist::Checklist;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, Frame};

/// Single source of truth: returns (component, height, is_focusable) for every visible item.
macro_rules! layout_items {
    ($self:ident) => {{
        let is_accel = $self.graphics_acceleration.checked();
        let wayland_checked = $self.wayland_socket.checked();
        let wayland_selector_active = wayland_checked && !$self.wayland_sockets.is_empty();
        let is_privileged = $self.privileged.checked();
        let has_gpus = !$self.discovered_gpus.is_empty();
        let all_drm = $self.gpu_all.checked();
        let has_gpu_access = is_accel && (all_drm || !$self.gpu_list.checked_indices().is_empty());
        let wayland_without_gpu = wayland_checked && !has_gpu_access;
        let gpu_count = $self.discovered_gpus.len() as u16;
        let gpu_all_warning_height = $self
            .gpu_all_warning
            .required_height($self.scroll_area.width.max(20));
        let wayland_gpu_note_height = $self
            .wayland_gpu_note
            .required_height($self.scroll_area.width.max(20));
        let privilege_warning_height = $self
            .privilege_warning
            .required_height($self.scroll_area.width.max(20));

        let mut items: Vec<(&mut dyn Component, u16, bool)> = Vec::new();

        if $self.hardware_scanning {
            items.push((&mut $self.scanning_indicator, 4, false));
        }

        items.push((&mut $self.graphics_acceleration, 3, true));

        if is_accel {
            items.push((&mut $self.gpu_all, 3, true));
        }

        if is_accel && all_drm {
            items.push((&mut $self.gpu_all_warning, gpu_all_warning_height, false));
        }

        if is_accel && has_gpus {
            let height = (gpu_count + 2).min(10);
            items.push((&mut $self.gpu_list, height, !all_drm));
        } else if is_accel && !has_gpus {
            items.push((&mut $self.gpu_empty, 3, false));
        }

        items.push((&mut $self.wayland_socket, 3, true));

        if wayland_selector_active {
            items.push((&mut $self.wayland_selector, 3, true));
        } else if wayland_checked {
            items.push((&mut $self.wayland_empty, 3, false));
        }

        if wayland_without_gpu {
            items.push((&mut $self.wayland_gpu_note, wayland_gpu_note_height, false));
        }

        items.push((&mut $self.privileged, 3, true));
        items.push((&mut $self.private_users, 3, true));

        if is_privileged {
            items.push((
                &mut $self.privilege_warning,
                privilege_warning_height,
                false,
            ));
        }

        items
    }};
}

/// Extract only focusable components (for keyboard navigation).
macro_rules! active_comps {
    ($self:ident) => {{
        layout_items!($self)
            .into_iter()
            .filter(|(_, _, f)| *f)
            .map(|(c, _, _)| c)
            .collect::<Vec<&mut dyn Component>>()
    }};
}

impl_wizard_nav!(PassthroughStepView, active_comps);

pub struct PassthroughStepView {
    graphics_acceleration: Checkbox,
    discovered_gpus: Vec<GpuDevice>,
    gpu_list: Checklist<GpuDevice>,
    gpu_all: Checkbox,
    gpu_all_warning: TextBlock,
    gpu_empty: TextBlock,
    wayland_socket: Checkbox,
    wayland_selector: RadioGroup,
    wayland_sockets: Vec<String>,
    wayland_empty: TextBlock,
    wayland_gpu_note: TextBlock,

    privileged: Checkbox,
    private_users: RadioGroup,
    privilege_warning: TextBlock,
    scanning_indicator: TextBlock,
    focus: FocusTracker,
    scroll_offset: u16,
    scroll_area: Rect,
    scroll_max: u16,
    hardware_scanning: bool,
    private_network: bool,
}

impl PassthroughStepView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_data: &PassthroughConfig,
        nw_mode: Option<NetworkMode>,
        wayland_sockets: Vec<String>,
        discovered_gpus: Vec<GpuDevice>,
        hardware_scanning: bool,
    ) -> Self {
        let is_host_nw = matches!(nw_mode, Some(NetworkMode::Host));
        let wayland_label = if is_host_nw {
            "Wayland Socket Passthrough"
        } else {
            "Wayland Socket Passthrough (Requires Host Network)"
        };

        let initial_wayland = if is_host_nw {
            initial_data.wayland_socket.is_some()
        } else {
            false
        };

        let wayland_options = if wayland_sockets.is_empty() {
            vec!["No sockets found".to_string()]
        } else {
            wayland_sockets.clone()
        };

        let initial_socket_idx = if let Some(saved_socket) = &initial_data.wayland_socket {
            wayland_sockets
                .iter()
                .position(|s| s == saved_socket)
                .unwrap_or(0)
        } else {
            0
        };

        let mut gpu_list = Checklist::new("Select Host GPU(s)", discovered_gpus.clone(), |gpu| {
            format!(
                "{} ({})",
                gpu.display_name,
                gpu.nodes.first().cloned().unwrap_or_default()
            )
        });

        let mut checked_indices = Vec::new();
        for (i, gpu) in discovered_gpus.iter().enumerate() {
            if gpu
                .nodes
                .iter()
                .any(|node| initial_data.device_binds.contains(node))
            {
                checked_indices.push(i);
            }
        }
        gpu_list.set_checked(checked_indices);
        let has_drm_devices = discovered_gpus
            .iter()
            .flat_map(|gpu| &gpu.nodes)
            .any(|node| node.starts_with("/dev/dri/"));

        let warning_text = "Enabled setting: [Exec] Capability=all. This grants every Linux capability, including system and mount administration, device and raw I/O access, network administration, kernel/module controls, and process tracing. PrivateUsers and networking remain controlled by their separate settings. A compromised container may take over the host.";

        let mut view = Self {
            graphics_acceleration: Checkbox::new(
                "Hardware Graphics Acceleration",
                initial_data.graphics_acceleration,
            ),
            discovered_gpus,
            gpu_list,
            gpu_all: Checkbox::new(
                "Passthrough all DRM devices (/dev/dri)",
                initial_data.gpu_passthrough_all && has_drm_devices,
            )
            .with_enabled(has_drm_devices),
            gpu_all_warning: TextBlock::new(
                " BROAD DEVICE ACCESS ",
                "Every host DRM device is exposed through /dev/dri. Prefer selecting one GPU when possible.",
            ),
            gpu_empty: TextBlock::new(
                " No GPUs Detected ",
                "No compatible GPU devices found on the host. Graphics acceleration may not work as expected.",
            ),
            wayland_socket: Checkbox::new(wayland_label, initial_wayland).with_enabled(is_host_nw),
            wayland_selector: RadioGroup::new("Source Socket", wayland_options, initial_socket_idx),
            wayland_sockets,
            wayland_empty: TextBlock::new(
                " No Wayland Sockets Detected ",
                "No Wayland display sockets found in the host runtime directory. Check if a Wayland compositor is running.",
            ),
            wayland_gpu_note: TextBlock::new(
                " SOCKET ONLY ",
                "Wayland is exposed without a GPU device. Applications may use software rendering; select a GPU or all DRM devices for hardware acceleration.",
            ),

            privileged: Checkbox::new("Privileged Mode (NOT RECOMMENDED)", initial_data.privileged),
            private_users: RadioGroup::new(
                "PrivateUsers (User Namespace)",
                vec![
                    "Default".to_string(),
                    "pick".to_string(),
                    "managed".to_string(),
                    "yes".to_string(),
                    "no".to_string(),
                ],
                match initial_data.private_users {
                    None => 0,
                    Some(PrivateUsersMode::Pick) => 1,
                    Some(PrivateUsersMode::Managed) => 2,
                    Some(PrivateUsersMode::Yes) => 3,
                    Some(PrivateUsersMode::No) => 4,
                    _ => 0,
                },
            ),
            privilege_warning: TextBlock::new("SECURITY RISK", warning_text),
            scanning_indicator: TextBlock::new(
                " SCANNING ",
                " [~] Hardware discovery is running in the background. GPU and NVIDIA lists will populate automatically when finished...",
            ),
            focus: FocusTracker::new(),
            scroll_offset: 0,
            scroll_area: Rect::default(),
            scroll_max: 0,
            hardware_scanning,
            private_network: nw_mode.as_ref().is_some_and(NetworkMode::is_private),
        };

        view.update_gpu_state();
        view.update_wayland_state();
        view.update_focus();
        view
    }

    fn update_wayland_state(&mut self) {
        let enabled = self.wayland_socket.checked() && !self.wayland_sockets.is_empty();
        self.wayland_selector.set_enabled(enabled);
    }

    fn update_gpu_state(&mut self) {
        self.gpu_list.set_enabled(!self.gpu_all.checked());
    }
}

impl Component for PassthroughStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Manual margin(1) — shrink area to match other step views
        let area = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        self.scroll_area = area;

        let items = layout_items!(self);

        // Map focus index -> visual index for scroll-to-active
        let focus_to_visual: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, (_, _, f))| *f)
            .map(|(i, _)| i)
            .collect();

        let total_height: u16 = items.iter().map(|(_, h, _)| h).sum();

        let mut item_ys: Vec<u16> = Vec::new();
        let mut y: u16 = 0;
        for (_, h, _) in &items {
            item_ys.push(y);
            y += h;
        }

        // Scroll to keep active item visible
        let active_vis_idx = focus_to_visual
            .get(self.focus.active_idx)
            .copied()
            .unwrap_or(0);
        let active_y = item_ys[active_vis_idx];
        let active_h = items[active_vis_idx].1;

        if active_y < self.scroll_offset {
            self.scroll_offset = active_y;
        } else if active_y + active_h > self.scroll_offset + area.height {
            let target = (active_y + active_h).saturating_sub(area.height);
            self.scroll_offset = item_ys
                .iter()
                .copied()
                .find(|&y| y >= target)
                .unwrap_or(target);
        }

        let max_scroll = total_height.saturating_sub(area.height);
        self.scroll_max = max_scroll;
        self.scroll_offset = self.scroll_offset.min(max_scroll);

        let inner_width = if total_height > area.height {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

        // Render visible items
        for (i, (comp, height, _)) in items.into_iter().enumerate() {
            let y = item_ys[i];
            if y + height <= self.scroll_offset || y >= self.scroll_offset + area.height {
                continue;
            }

            let screen_y = area.y + y - self.scroll_offset;
            let draw_height = height.min(area.y + area.height - screen_y);
            let draw_rect = Rect::new(area.x, screen_y, inner_width, draw_height);
            comp.render(f, draw_rect);
        }

        // Scrollbar
        if total_height > area.height {
            use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
            let mut state = ScrollbarState::new(total_height as usize)
                .position(self.scroll_offset as usize)
                .viewport_content_length(usize::from(area.height));
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            f.render_stateful_widget(
                scrollbar,
                Rect {
                    x: area.x + area.width - 1,
                    y: area.y,
                    width: 1,
                    height: area.height,
                },
                &mut state,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let res = delegate_wizard_navigation!(self, key, active_comps);

        if matches!(
            key.code,
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Left | KeyCode::Right
        ) {
            self.update_gpu_state();
            self.update_wayland_state();
            self.update_focus();
        }

        res
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if mouse.column < self.scroll_area.x
            || mouse.column >= self.scroll_area.x.saturating_add(self.scroll_area.width)
            || mouse.row < self.scroll_area.y
            || mouse.row >= self.scroll_area.y.saturating_add(self.scroll_area.height)
        {
            return EventResult::Ignored;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                EventResult::Consumed
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(3).min(self.scroll_max);
                EventResult::Consumed
            }
            _ => EventResult::Ignored,
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.private_users.selected_idx() == 2 && !self.private_network {
            return Err("PrivateUsers=managed requires a private network mode".into());
        }
        Ok(())
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }
}

impl StepComponent for PassthroughStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.graphics_acceleration = self.graphics_acceleration.checked();
        ctx.passthrough.gpu_passthrough_all =
            self.graphics_acceleration.checked() && self.gpu_all.checked();
        ctx.passthrough.privileged = self.privileged.checked();
        ctx.passthrough.private_users = match self.private_users.selected_idx() {
            0 => None,
            1 => Some(PrivateUsersMode::Pick),
            2 => Some(PrivateUsersMode::Managed),
            3 => Some(PrivateUsersMode::Yes),
            4 => Some(PrivateUsersMode::No),
            _ => None,
        };

        let mut selected_nodes = Vec::new();
        if self.graphics_acceleration.checked() && !self.gpu_all.checked() {
            for &idx in self.gpu_list.checked_indices() {
                if let Some(gpu) = self.discovered_gpus.get(idx) {
                    selected_nodes.extend(gpu.nodes.clone());
                }
            }
        }
        ctx.passthrough.selected_gpu_nodes = selected_nodes;

        let is_host_nw = matches!(
            ctx.network.network_mode(),
            Some(crate::nspawn::models::NetworkMode::Host)
        );

        if self.wayland_socket.checked() && is_host_nw && !self.wayland_sockets.is_empty() {
            let idx = self.wayland_selector.selected_idx();
            ctx.passthrough.wayland_socket = Some(self.wayland_sockets[idx].clone());
        } else {
            ctx.passthrough.wayland_socket = None;
        }
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn privileged_warning_names_the_effective_setting_and_independent_controls() {
        crate::ui::theme::init_theme(crate::ui::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: true,
            private_users: None,
            graphics_acceleration: false,
            gpu_passthrough_all: false,
            wayland_socket: None,
            nvidia_gpu: false,
            nvidia_profile: None,
        };
        let mut view = PassthroughStepView::new(
            &config,
            Some(NetworkMode::Host),
            Vec::new(),
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
    fn all_drm_option_is_explicit_and_disables_individual_gpu_selection() {
        crate::ui::theme::init_theme(crate::ui::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: false,
            private_users: None,
            graphics_acceleration: true,
            gpu_passthrough_all: true,
            wayland_socket: Some("wayland-0".into()),
            nvidia_gpu: false,
            nvidia_profile: None,
        };
        let gpu = GpuDevice {
            display_name: "Test GPU".into(),
            driver_type: "DRM/KMS".into(),
            nodes: vec!["/dev/dri/card0".into(), "/dev/dri/renderD128".into()],
        };
        let mut view = PassthroughStepView::new(
            &config,
            Some(NetworkMode::Host),
            vec!["wayland-0".into()],
            vec![gpu],
            false,
        );

        assert!(view.gpu_all.checked());
        assert!(!view.gpu_list.is_enabled());

        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
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

        assert!(rendered.contains("Passthrough all DRM devices (/dev/dri)"));
        assert!(rendered.contains("Every host DRM device is exposed"));
        assert!(!rendered.contains("SOCKET ONLY"));
    }
}
