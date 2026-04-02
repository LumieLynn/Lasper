use crate::nspawn::models::NetworkMode;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub struct PassthroughStepView {
    generic_gpu: Checkbox,
    wayland_socket: Checkbox,
    nvidia_gpu: Checkbox,
    focus: FocusTracker,
}

impl PassthroughStepView {
    pub fn new(
        initial_data: &PassthroughConfig,
        nw_mode: Option<NetworkMode>,
        nvidia_toolkit_installed: bool,
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
            initial_data.wayland_socket
        } else {
            false
        };

        let mut view = Self {
            generic_gpu: Checkbox::new(
                "Generic GPU Passthrough (/dev/dri, /dev/mali)",
                initial_data.full_capabilities,
            ),
            wayland_socket: Checkbox::new(wayland_label, initial_wayland).with_enabled(is_host_nw),
            nvidia_gpu: Checkbox::new(nvidia_label, initial_data.nvidia_gpu)
                .with_enabled(nvidia_toolkit_installed),
            focus: FocusTracker::new(),
        };

        view.update_focus();
        view
    }

    fn update_focus(&mut self) {
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.generic_gpu,
            &mut self.wayland_socket,
            &mut self.nvidia_gpu,
        ];
        self.focus.update_focus(&mut components, true);
    }

    fn get_components(&self) -> Vec<&dyn Component> {
        vec![&self.generic_gpu, &self.wayland_socket, &self.nvidia_gpu]
    }
}

impl Component for PassthroughStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let constraints = vec![
            Constraint::Length(3), // Generic
            Constraint::Length(3), // Wayland
            Constraint::Length(3), // NVIDIA
            Constraint::Min(0),
        ];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        self.generic_gpu.render(f, chunks[0]);
        self.wayland_socket.render(f, chunks[1]);
        self.nvidia_gpu.render(f, chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Tab => {
                let comps: Vec<&dyn Component> =
                    vec![&self.generic_gpu, &self.wayland_socket, &self.nvidia_gpu];
                self.focus.next(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                let comps: Vec<&dyn Component> =
                    vec![&self.generic_gpu, &self.wayland_socket, &self.nvidia_gpu];
                self.focus.prev(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            _ => {}
        }

        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.generic_gpu,
            &mut self.wayland_socket,
            &mut self.nvidia_gpu,
        ];

        let res = comps[self.focus.active_idx].handle_key(key);
        match res {
            EventResult::FocusNext => {
                let comps: Vec<&dyn Component> =
                    vec![&self.generic_gpu, &self.wayland_socket, &self.nvidia_gpu];
                self.focus.next(&comps);
                self.update_focus();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                let comps: Vec<&dyn Component> =
                    vec![&self.generic_gpu, &self.wayland_socket, &self.nvidia_gpu];
                self.focus.prev(&comps);
                self.update_focus();
                EventResult::Consumed
            }
            _ => res,
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for PassthroughStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.full_capabilities = self.generic_gpu.checked();
        ctx.passthrough.nvidia_gpu = self.nvidia_gpu.checked();

        let is_host_nw = matches!(ctx.network.network_mode(), Some(NetworkMode::Host));
        ctx.passthrough.wayland_socket = self.wayland_socket.checked() && is_host_nw;
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
