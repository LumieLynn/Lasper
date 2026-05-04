use crate::nspawn::models::{BindMount, IdmapSuffix};
use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::widgets::lists::editable_list::EditableList;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::ui::wizard::StepAction;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

pub struct DevicesStepView {
    bind_list: EditableList<BindMount>,
    nvidia_enabled: bool,
}

impl DevicesStepView {
    pub fn new(initial_data: &PassthroughConfig) -> Self {
        Self {
            bind_list: EditableList::new(
                " Configured Bind Mounts ",
                initial_data.bind_mounts.clone(),
                |bm| {
                    let suffix_display = if bm.suffix == IdmapSuffix::None {
                        String::new()
                    } else {
                        format!(" [{}]", bm.suffix.label())
                    };
                    format!(
                        "  {}:{} ({}){}",
                        bm.source,
                        bm.target,
                        if bm.readonly { "ro" } else { "rw" },
                        suffix_display
                    )
                },
                |idx| AppMessage::Wizard(WizardMessage::BindMountRemoved(idx)),
            ),

            nvidia_enabled: initial_data.nvidia_gpu,
        }
    }
}

impl Component for DevicesStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(4), // NVIDIA status
                Constraint::Min(0),    // List
                Constraint::Length(1), // Hint
            ])
            .split(area);

        let nvidia_status = if self.nvidia_enabled {
            Paragraph::new("\n  NVIDIA GPU Passthrough is ENABLED.\n  (Lasper manages drivers via JIT assembly)").style(Style::default().fg(Color::Cyan))
        } else {
            Paragraph::new("\n  NVIDIA passthrough is disabled.")
        };
        f.render_widget(nvidia_status, chunks[0]);

        self.bind_list.set_focus(true);
        self.bind_list.render(f, chunks[1]);

        let footer = " [A]dd mount, [E]dit mount, [D]elete mount, [Enter] next ";
        f.render_widget(
            Paragraph::new(footer).style(Style::default().fg(Color::Yellow)),
            chunks[2],
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                return EventResult::Message(AppMessage::Wizard(
                    WizardMessage::OpenBindDialog,
                ));
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                if let Some(bm) = self.bind_list.selected_item() {
                    let idx = self.bind_list.selected();
                    return EventResult::Message(AppMessage::Wizard(
                        WizardMessage::OpenBindEditDialog(idx, bm.clone()),
                    ));
                }
            }
            _ => {}
        }

        self.bind_list.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.bind_list.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.bind_list.is_focused()
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for DevicesStepView {
    fn handle_message(&mut self, msg: &AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(WizardMessage::BindMountAdded(bm)) => {
                self.bind_list.add_item(bm.clone());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::BindMountUpdated(idx, bm)) => {
                self.bind_list.update_item(*idx, bm.clone());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => StepAction::CloseDialog,
            _ => StepAction::None,
        }
    }

    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.bind_mounts = self.bind_list.items().to_vec();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
