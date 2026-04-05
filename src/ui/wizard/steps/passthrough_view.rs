use crate::nspawn::models::NetworkMode;
use crate::nspawn::utils::scan_available_wayland_sockets;
use crate::ui::core::{Component, EventResult, FocusTracker};
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let wayland_selector_active = $self.wayland_socket.checked() && !$self.wayland_sockets.is_empty();
        let mut comps: Vec<&mut dyn Component> = vec![
            &mut $self.generic_gpu,
            &mut $self.wayland_socket,
        ];

        if wayland_selector_active {
            comps.push(&mut $self.wayland_selector);
        }

        comps.push(&mut $self.nvidia_gpu);
        comps
    }};
}

pub struct PassthroughStepView {
    generic_gpu: Checkbox,
    wayland_socket: Checkbox,
    wayland_selector: RadioGroup,
    wayland_sockets: Vec<String>,
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
            initial_data.wayland_socket.is_some()
        } else {
            false
        };

        let wayland_sockets = scan_available_wayland_sockets();
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

        let mut view = Self {
            generic_gpu: Checkbox::new(
                "Generic GPU Passthrough (/dev/dri, /dev/mali)",
                initial_data.full_capabilities,
            ),
            wayland_socket: Checkbox::new(wayland_label, initial_wayland).with_enabled(is_host_nw),
            wayland_selector: RadioGroup::new("Source Socket", wayland_options, initial_socket_idx),
            wayland_sockets,
            nvidia_gpu: Checkbox::new(nvidia_label, initial_data.nvidia_gpu)
                .with_enabled(nvidia_toolkit_installed),
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

    fn update_focus(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.update_focus(&mut comps, true);
    }

    fn next(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.next(&mut comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.prev(&mut comps);
        self.update_focus();
    }
}

impl Component for PassthroughStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let mut constraints = vec![
            Constraint::Length(3), // Generic
            Constraint::Length(3), // Wayland checkbox
        ];

        if self.wayland_socket.checked() {
            constraints.push(Constraint::Length(3)); // Wayland selector
        }

        constraints.push(Constraint::Length(3)); // NVIDIA
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut current_idx = 0;
        self.generic_gpu.render(f, chunks[current_idx]);
        current_idx += 1;

        self.wayland_socket.render(f, chunks[current_idx]);
        current_idx += 1;

        if self.wayland_socket.checked() {
            self.wayland_selector.render(f, chunks[current_idx]);
            current_idx += 1;
        }

        self.nvidia_gpu.render(f, chunks[current_idx]);
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

        let mut comps = active_comps!(self);
        let res = comps[self.focus.active_idx].handle_key(key);

        // If the toggle changed, we might need to update focus or state
        if matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
            self.update_wayland_state();
            self.update_focus();
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

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for PassthroughStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.full_capabilities = self.generic_gpu.checked();
        ctx.passthrough.nvidia_gpu = self.nvidia_gpu.checked();

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
