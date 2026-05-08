use crate::nspawn::models::NetworkMode;
use crate::nspawn::platform::gpu::GpuDevice;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::lists::checklist::Checklist;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{layout::Rect, Frame};

/// Single source of truth: returns (component, height, is_focusable) for every visible item.
macro_rules! layout_items {
    ($self:ident) => {{
        let is_accel = $self.graphics_acceleration.checked();
        let wayland_checked = $self.wayland_socket.checked();
        let wayland_selector_active = wayland_checked && !$self.wayland_sockets.is_empty();
        let is_privileged = $self.privileged.checked();
        let has_gpus = !$self.discovered_gpus.is_empty();
        let gpu_count = $self.discovered_gpus.len() as u16;

        let mut items: Vec<(&mut dyn Component, u16, bool)> = Vec::new();

        if $self.hardware_scanning {
            items.push((&mut $self.scanning_indicator, 4, false));
        }

        items.push((&mut $self.graphics_acceleration, 3, true));

        if is_accel && has_gpus {
            let height = (gpu_count + 2).min(10);
            items.push((&mut $self.gpu_list, height, true));
        }

        items.push((&mut $self.wayland_socket, 3, true));

        if wayland_selector_active {
            items.push((&mut $self.wayland_selector, 3, true));
        }

        items.push((&mut $self.privileged, 3, true));
        items.push((&mut $self.private_users, 3, true));

        if is_privileged {
            items.push((&mut $self.privilege_warning, 5, false));
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
    wayland_socket: Checkbox,
    wayland_selector: RadioGroup,
    wayland_sockets: Vec<String>,

    privileged: Checkbox,
    private_users: RadioGroup,
    privilege_warning: TextBlock,
    scanning_indicator: TextBlock,
    focus: FocusTracker,
    scroll_offset: u16,
    hardware_scanning: bool,
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

        let warning_text = " [!] WARNING: Privileged mode grants the container full host root capabilities. This allows the container to potentially take over the host system. Use only if standard passthrough fails and you trust the container payload.";

        let mut view = Self {
            graphics_acceleration: Checkbox::new(
                "Hardware Graphics Acceleration",
                initial_data.graphics_acceleration,
            ),
            discovered_gpus,
            gpu_list,
            wayland_socket: Checkbox::new(wayland_label, initial_wayland).with_enabled(is_host_nw),
            wayland_selector: RadioGroup::new("Source Socket", wayland_options, initial_socket_idx),
            wayland_sockets,

            privileged: Checkbox::new("Privileged Mode (NOT RECOMMENDED)", initial_data.privileged),
            private_users: RadioGroup::new(
                "PrivateUsers (User Namespace)",
                vec![
                    "Default (systemd)".to_string(),
                    "pick".to_string(),
                    "Enabled (yes)".to_string(),
                    "Disabled (no)".to_string(),
                ],
                match &initial_data.private_users {
                    None => 0,
                    Some(v) if v == "pick" => 1,
                    Some(v) if v == "yes" => 2,
                    Some(v) if v == "no" => 3,
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
            hardware_scanning,
        };

        view.update_wayland_state();
        view.update_focus();
        view
    }

    fn update_wayland_state(&mut self) {
        let enabled = self.wayland_socket.checked() && !self.wayland_sockets.is_empty();
        self.wayland_selector.set_enabled(enabled);
    }
}

impl Component for PassthroughStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
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
            let mut state =
                ScrollbarState::new(max_scroll as usize).position(self.scroll_offset as usize);
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
            self.update_wayland_state();
            self.update_focus();
        }

        res
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for PassthroughStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.graphics_acceleration = self.graphics_acceleration.checked();
        ctx.passthrough.privileged = self.privileged.checked();
        ctx.passthrough.private_users = match self.private_users.selected_idx() {
            0 => None,
            1 => Some("pick".into()),
            2 => Some("yes".into()),
            3 => Some("no".into()),
            _ => None,
        };

        let mut selected_nodes = Vec::new();
        if self.graphics_acceleration.checked() {
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
