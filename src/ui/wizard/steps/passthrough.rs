use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Paragraph},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::checkbox::Checkbox;
use crate::nspawn::StatusLevel;
use crate::nspawn::nvidia::detect_nvidia;
use async_trait::async_trait;

pub struct PassthroughStep;

impl PassthroughStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for PassthroughStep {
    fn title(&self) -> String { "Hardware & Display Passthrough".into() }

    fn next_step(&self, _context: &WizardContext) -> Option<Box<dyn IStep>> {
        Some(Box::new(crate::ui::wizard::steps::devices::DevicesStep::new()))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(4), // instructions
                Constraint::Length(1),
                Constraint::Length(3), // Generic
                Constraint::Length(1),
                Constraint::Length(3), // Wayland
                Constraint::Length(1),
                Constraint::Length(3), // NVIDIA
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        f.render_widget(Paragraph::new("  Select quick passthrough options for this container.  \n  (NVIDIA will perform a host scan upon proceeding)").style(Style::default().fg(Color::White)), chunks[0]);

        Checkbox::new("Generic GPU Passthrough (/dev/dri, /dev/mali)", context.passthrough.generic_gpu)
            .focused(context.passthrough.field_idx == 0)
            .render(f, chunks[2]);

        let wayland_label = if context.network.mode == 0 { "Wayland Socket Passthrough" } else { "Wayland (Requires Host Network)" };
        Checkbox::new(wayland_label, context.passthrough.wayland_socket)
            .focused(context.passthrough.field_idx == 1)
            .render(f, chunks[4]);

        Checkbox::new("NVIDIA Driver & GPU Passthrough (Scan host)", context.passthrough.nvidia_enabled)
            .focused(context.passthrough.field_idx == 2)
            .render(f, chunks[6]);

        render_hint(f, chunks[8], &["[Space/Tab] toggle", "[↑/↓] select", "[Enter] apply & next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Up => {
                if context.passthrough.field_idx > 0 { context.passthrough.field_idx -= 1; }
                StepAction::None
            }
            KeyCode::Down | KeyCode::Tab => {
                context.passthrough.field_idx = (context.passthrough.field_idx + 1) % 3;
                StepAction::None
            }
            KeyCode::Char(' ') => {
                if context.passthrough.field_idx == 2 {
                    context.passthrough.nvidia_enabled = !context.passthrough.nvidia_enabled;
                    if context.passthrough.nvidia_enabled && !context.passthrough.nvidia_loaded {
                        return StepAction::Status("Detecting NVIDIA hardware...".into(), StatusLevel::Info);
                    }
                } else if context.passthrough.field_idx == 0 {
                    context.passthrough.generic_gpu = !context.passthrough.generic_gpu;
                } else if context.passthrough.field_idx == 1 && context.network.mode == 0 {
                    context.passthrough.wayland_socket = !context.passthrough.wayland_socket;
                }
                StepAction::None
            }
            KeyCode::Enter => {
                if context.passthrough.nvidia_enabled && !context.passthrough.nvidia_loaded {
                    // Try to load it now
                    let info = detect_nvidia().await;
                    context.passthrough.nvidia = info;
                    context.passthrough.nvidia_loaded = true;
                    context.passthrough.nvidia_devices_sel = vec![true; context.passthrough.nvidia.devices.len()];
                    context.passthrough.nvidia_sysro_sel = vec![true; context.passthrough.nvidia.system_ro.len()];
                    context.passthrough.nvidia_libs_sel = vec![true; context.passthrough.nvidia.driver_files.len()];
                    StepAction::Next
                } else {
                    StepAction::Next
                }
            }
            _ => StepAction::None,
        }
    }
}
