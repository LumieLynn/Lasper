use crate::nspawn::models::NetworkMode;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::widgets::lists::checklist::Checklist;
use crate::nspawn::hw::gpu::GpuDevice;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let is_accel = $self.graphics_acceleration.checked();
        let wayland_socket_checked = $self.wayland_socket.checked();
        let wayland_selector_active = wayland_socket_checked && !$self.wayland_sockets.is_empty();

        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.graphics_acceleration];

        if is_accel && !$self.discovered_gpus.is_empty() {
            comps.push(&mut $self.gpu_list);
        }

        comps.push(&mut $self.wayland_socket);

        if wayland_selector_active {
            comps.push(&mut $self.wayland_selector);
        }

        comps.push(&mut $self.nvidia_gpu);
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
    privileged: Checkbox,
    privilege_warning: TextBlock,
    focus: FocusTracker,
}

impl PassthroughStepView {
    pub fn new(
        initial_data: &PassthroughConfig,
        nw_mode: Option<NetworkMode>,
        nvidia_toolkit_installed: bool,
        wayland_sockets: Vec<String>,
        discovered_gpus: Vec<GpuDevice>,
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
            format!("{} ({})", gpu.display_name, gpu.nodes.first().cloned().unwrap_or_default())
        });

        // Pre-check previously selected GPUs
        let mut checked_indices = Vec::new();
        for (i, gpu) in discovered_gpus.iter().enumerate() {
            if gpu.nodes.iter().any(|node| initial_data.device_binds.contains(node)) {
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
            nvidia_gpu: Checkbox::new(nvidia_label, initial_data.nvidia_gpu)
                .with_enabled(nvidia_toolkit_installed),
            privileged: Checkbox::new("Privileged Mode (NOT RECOMMENDED)", initial_data.privileged),
            privilege_warning: TextBlock::new("SECURITY RISK", warning_text),
            focus: FocusTracker::new(),
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
        let mut constraints = vec![
            Constraint::Length(3), // Acceleration
        ];

        if self.graphics_acceleration.checked() && !self.discovered_gpus.is_empty() {
             // Checklist needs +2 lines for borders, limited to a reasonable height
            let height = (self.discovered_gpus.len() as u16 + 2).min(10);
            constraints.push(Constraint::Length(height));
        }

        constraints.push(Constraint::Length(3)); // Wayland checkbox

        if self.wayland_socket.checked() {
            constraints.push(Constraint::Length(3)); // Wayland selector
        }

        constraints.push(Constraint::Length(3)); // NVIDIA
        constraints.push(Constraint::Length(3)); // Privileged
        
        if self.privileged.checked() {
            constraints.push(Constraint::Length(5)); // Warning + Borders
        }
        
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut current_idx = 0;
        self.graphics_acceleration.render(f, chunks[current_idx]);
        current_idx += 1;

        if self.graphics_acceleration.checked() && !self.discovered_gpus.is_empty() {
            self.gpu_list.render(f, chunks[current_idx]);
            current_idx += 1;
        }

        self.wayland_socket.render(f, chunks[current_idx]);
        current_idx += 1;

        if self.wayland_socket.checked() {
            self.wayland_selector.render(f, chunks[current_idx]);
            current_idx += 1;
        }

        self.nvidia_gpu.render(f, chunks[current_idx]);
        current_idx += 1;

        self.privileged.render(f, chunks[current_idx]);
        current_idx += 1;

        if self.privileged.checked() {
            self.privilege_warning.render(f, chunks[current_idx]);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let res = delegate_wizard_navigation!(self, key, active_comps);

        // If the toggle changed, we might need to update focus or state
        if matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
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

        let is_host_nw = matches!(ctx.network.network_mode(), Some(crate::nspawn::models::NetworkMode::Host));
        
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
