use crate::nspawn::models::NetworkMode;
use crate::nspawn::platform::gpu::GpuDevice;
use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
use crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::lists::checklist::Checklist;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use std::collections::HashMap;

macro_rules! active_comps {
    ($self:ident) => {{
        let is_accel = $self.graphics_acceleration.checked();
        let wayland_socket_checked = $self.wayland_socket.checked();
        let wayland_selector_active = wayland_socket_checked && !$self.wayland_sockets.is_empty();
        let nvidia_enabled = $self.nvidia_gpu.checked();

        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.graphics_acceleration];

        if is_accel && !$self.discovered_gpus.is_empty() {
            comps.push(&mut $self.gpu_list);
        }

        comps.push(&mut $self.wayland_socket);

        if wayland_selector_active {
            comps.push(&mut $self.wayland_selector);
        }

        comps.push(&mut $self.nvidia_gpu);

        if nvidia_enabled {
            let mode_idx = $self.nvidia_mode_selector.selected_idx();
            comps.push(&mut $self.nvidia_device_selector);
            comps.push(&mut $self.nvidia_mode_selector);
            if mode_idx == 1 {
                for (_, tb) in &mut $self.nvidia_dest_inputs {
                    comps.push(tb);
                }
            }
            comps.push(&mut $self.nvidia_inject_env);
        }

        comps.push(&mut $self.privileged);
        comps
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
    nvidia_gpu: Checkbox,

    // NVIDIA advanced
    nvidia_device_selector: RadioGroup,
    nvidia_mode_selector: RadioGroup,
    nvidia_dest_inputs: Vec<(NvidiaFileCategory, TextBox)>,
    nvidia_inject_env: Checkbox,

    privileged: Checkbox,
    privilege_warning: TextBlock,
    focus: FocusTracker,
    scroll_offset: u16,
}

impl PassthroughStepView {
    pub fn new(
        initial_data: &PassthroughConfig,
        nw_mode: Option<NetworkMode>,
        nvidia_toolkit_installed: bool,
        wayland_sockets: Vec<String>,
        discovered_gpus: Vec<GpuDevice>,
        nvidia_available_devices: Vec<String>,
        active_nvidia_categories: Vec<NvidiaFileCategory>,
    ) -> Self {
        let nvidia_label = if nvidia_toolkit_installed {
            "NVIDIA Driver & GPU Passthrough (Scan host)"
        } else {
            "NVIDIA Driver & GPU Passthrough (Missing: nvidia-container-toolkit)"
        };

        let is_host_nw = matches!(nw_mode, Some(NetworkMode::Host));
        let wayland_label = if is_host_nw {
            "Wayland Socket Passthrough"
        } else {
            "Wayland Socket Passthrough (Requires Host Network)"
        };

        // Strict enforcement: if not host network, wayland_socket must be false
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

        // Determine initial index for selector
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

        // Pre-check previously selected GPUs
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

        let nvidia_profile = initial_data.nvidia_profile.as_ref();
        let nvidia_mode = nvidia_profile
            .map(|p| p.mode.clone())
            .unwrap_or(NvidiaPassthroughMode::Mirror);
        let nvidia_device = nvidia_profile
            .map(|p| p.gpu_device.clone())
            .unwrap_or("all".to_string());
        let nvidia_inject_env = nvidia_profile.map(|p| p.inject_env).unwrap_or(true);
        let saved_dests = nvidia_profile
            .map(|p| p.category_destinations.clone())
            .unwrap_or_default();

        let device_idx = nvidia_available_devices
            .iter()
            .position(|d| d == &nvidia_device)
            .unwrap_or(0);
        let mode_idx = match nvidia_mode {
            NvidiaPassthroughMode::Mirror => 0,
            NvidiaPassthroughMode::Categorized => 1,
        };

        // Merge static essential categories with dynamic categories detected from host
        let mut display_categories = NvidiaFileCategory::all_static();
        for cat in active_nvidia_categories {
            if !display_categories.contains(&cat) {
                display_categories.push(cat);
            }
        }
        display_categories.sort_by_key(|c| format!("{:?}", c));

        let mut nvidia_dest_inputs = Vec::new();
        for cat in display_categories {
            let default_dest = match cat {
                NvidiaFileCategory::Lib64 => "/usr/lib",
                NvidiaFileCategory::Lib32 => "/usr/lib32",
                NvidiaFileCategory::Bin => "/usr/bin",
                NvidiaFileCategory::Firmware => "/lib/firmware/nvidia",
                NvidiaFileCategory::Config => "/usr/share",
                NvidiaFileCategory::Xorg => "/usr/lib/xorg/modules",
                NvidiaFileCategory::Vdpau => "/usr/lib/vdpau",
                NvidiaFileCategory::Gbm => "/usr/lib/gbm",
            };
            let dest = saved_dests
                .get(&cat)
                .cloned()
                .unwrap_or(default_dest.to_string());
            nvidia_dest_inputs.push((cat.clone(), TextBox::new(cat.label(), dest)));
        }

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
            nvidia_gpu: Checkbox::new(nvidia_label, initial_data.nvidia_gpu)
                .with_enabled(nvidia_toolkit_installed),

            nvidia_device_selector: RadioGroup::new(
                "GPU Device",
                nvidia_available_devices,
                device_idx,
            ),
            nvidia_mode_selector: RadioGroup::new(
                "Passthrough Mode",
                vec!["Mirror Host".to_string(), "Categorized".to_string()],
                mode_idx,
            ),
            nvidia_dest_inputs,
            nvidia_inject_env: Checkbox::new(
                "Inject environment variables (/etc/environment)",
                nvidia_inject_env,
            ),

            privileged: Checkbox::new("Privileged Mode (NOT RECOMMENDED)", initial_data.privileged),
            privilege_warning: TextBlock::new("SECURITY RISK", warning_text),
            focus: FocusTracker::new(),
            scroll_offset: 0,
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
        let is_accel = self.graphics_acceleration.checked();
        let wayland_checked = self.wayland_socket.checked();
        let nvidia_enabled = self.nvidia_gpu.checked();
        let nvidia_mode = self.nvidia_mode_selector.selected_idx();
        let is_privileged = self.privileged.checked();
        let has_gpus = !self.discovered_gpus.is_empty();
        let gpu_count = self.discovered_gpus.len() as u16;

        let mut visual_items: Vec<(&mut dyn Component, u16)> = Vec::new();

        visual_items.push((&mut self.graphics_acceleration, 3));
        if is_accel && has_gpus {
            let height = (gpu_count + 2).min(10);
            visual_items.push((&mut self.gpu_list, height));
        }

        visual_items.push((&mut self.wayland_socket, 3));
        if wayland_checked {
            visual_items.push((&mut self.wayland_selector, 3));
        }

        visual_items.push((&mut self.nvidia_gpu, 3));
        if nvidia_enabled {
            visual_items.push((&mut self.nvidia_device_selector, 3));
            visual_items.push((&mut self.nvidia_mode_selector, 3));
            if nvidia_mode == 1 {
                for (_, tb) in &mut self.nvidia_dest_inputs {
                    visual_items.push((tb, 3));
                }
            }
            visual_items.push((&mut self.nvidia_inject_env, 3));
        }

        visual_items.push((&mut self.privileged, 3));
        if is_privileged {
            visual_items.push((&mut self.privilege_warning, 5));
        }

        let mut total_height = 0;
        let mut item_ys = Vec::new();
        for (_, h) in &visual_items {
            item_ys.push(total_height);
            total_height += h;
        }

        // Auto-scroll logic: ensure the active focusable component is completely visible
        let active_vis_idx = self.focus.active_idx.min(visual_items.len().saturating_sub(1));
        let active_y = item_ys[active_vis_idx];
        let active_h = visual_items[active_vis_idx].1;

        if active_y < self.scroll_offset {
            self.scroll_offset = active_y;
        } else if active_y + active_h > self.scroll_offset + area.height {
            let target_scroll = (active_y + active_h).saturating_sub(area.height);
            let new_scroll = item_ys.iter().copied().find(|&y| y >= target_scroll).unwrap_or(target_scroll);
            self.scroll_offset = new_scroll;
        }

        let max_scroll = total_height.saturating_sub(area.height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);

        // Rendering logic
        let inner_width = if total_height > area.height { area.width.saturating_sub(1) } else { area.width };

        for (i, (comp, height)) in visual_items.into_iter().enumerate() {
            let y = item_ys[i];
            
            if y + height <= self.scroll_offset {
                continue; // completely above view
            }
            if y >= self.scroll_offset + area.height {
                continue; // completely below view
            }
            if y < self.scroll_offset {
                continue; // partially above view, skip to prevent bad rect bounds
            }
            
            let screen_y = area.y + y - self.scroll_offset;
            let draw_height = height.min(area.y + area.height - screen_y);
            
            let draw_rect = Rect::new(area.x, screen_y, inner_width, draw_height);
            comp.render(f, draw_rect);
        }

        if total_height > area.height {
            use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
            let mut state = ScrollbarState::new(max_scroll as usize).position(self.scroll_offset as usize);
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));
            
            f.render_stateful_widget(
                scrollbar,
                Rect { x: area.x + area.width - 1, y: area.y, width: 1, height: area.height },
                &mut state
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let res = delegate_wizard_navigation!(self, key, active_comps);

        // If the toggle changed, we might need to update focus or state
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
        if self.nvidia_gpu.checked() && self.nvidia_mode_selector.selected_idx() == 1 {
            for (cat, tb) in &self.nvidia_dest_inputs {
                let text = tb.value().trim();
                if text.is_empty() {
                    return Err(format!(
                        "Destination path for {} cannot be empty",
                        cat.label()
                    ));
                }
                if !text.starts_with('/') {
                    return Err(format!(
                        "Destination path for {} must be an absolute path (start with '/')",
                        cat.label()
                    ));
                }
                if text.starts_with("/dev/")
                    || text.starts_with("/proc/")
                    || text.starts_with("/sys/")
                {
                    return Err(format!(
                        "Destination path for {} cannot be in /dev, /proc, or /sys",
                        cat.label()
                    ));
                }
            }
        }
        Ok(())
    }
}

impl StepComponent for PassthroughStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.graphics_acceleration = self.graphics_acceleration.checked();
        ctx.passthrough.nvidia_gpu = self.nvidia_gpu.checked();
        ctx.passthrough.privileged = self.privileged.checked();

        let mut selected_nodes = Vec::new();
        if self.graphics_acceleration.checked() {
            for &idx in self.gpu_list.checked_indices() {
                if let Some(gpu) = self.discovered_gpus.get(idx) {
                    selected_nodes.extend(gpu.nodes.clone());
                }
            }
        }
        ctx.passthrough.selected_gpu_nodes = selected_nodes;

        // NVIDIA advanced
        if self.nvidia_gpu.checked() {
            ctx.passthrough.nvidia_gpu_device = self.nvidia_device_selector.options()
                [self.nvidia_device_selector.selected_idx()]
            .clone();
            ctx.passthrough.nvidia_passthrough_mode =
                if self.nvidia_mode_selector.selected_idx() == 0 {
                    NvidiaPassthroughMode::Mirror
                } else {
                    NvidiaPassthroughMode::Categorized
                };
            ctx.passthrough.nvidia_inject_env = self.nvidia_inject_env.checked();

            let mut dests = HashMap::new();
            for (cat, tb) in &self.nvidia_dest_inputs {
                dests.insert(cat.clone(), tb.value().to_string());
            }
            ctx.passthrough.nvidia_category_destinations = dests;
        }

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
