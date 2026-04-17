use crate::nspawn::models::BindMount;
use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::widgets::composites::bind_mount::BindMountBox;
use crate::ui::widgets::lists::editable_list::EditableList;
use crate::ui::wizard::context::{PassthroughConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::ui::wizard::core::render_editor_overlay;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

pub struct DevicesStepView {
    bind_list: EditableList<BindMount>,
    bind_editor: Option<BindMountBox>,
    nvidia_enabled: bool,
}

impl DevicesStepView {
    pub fn new(initial_data: &PassthroughConfig) -> Self {
        Self {
            bind_list: EditableList::new(
                " Configured Bind Mounts ",
                initial_data.bind_mounts.clone(),
                |bm| {
                    format!(
                        "  {}:{} ({})",
                        bm.source,
                        bm.target,
                        if bm.readonly { "ro" } else { "rw" }
                    )
                },
                |idx| AppMessage::Wizard(WizardMessage::BindMountRemoved(idx)),
            ),

            bind_editor: None,
            nvidia_enabled: initial_data.nvidia_gpu,
        }
    }
}

impl Component for DevicesStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        if let Some(editor) = &mut self.bind_editor {
            render_editor_overlay(f, "Add Bind Mount", 60, 60, editor);
            return;
        }

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

        let footer = " [A]dd mount, [D]elete mount, [Enter] next ";
        f.render_widget(
            Paragraph::new(footer).style(Style::default().fg(Color::Yellow)),
            chunks[2],
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if let Some(editor) = &mut self.bind_editor {
            if key.code == KeyCode::Esc {
                self.bind_editor = None;
                return EventResult::Consumed;
            }
            let res = editor.handle_key(key);
            match &res {
                EventResult::Message(AppMessage::Wizard(WizardMessage::BindMountAdded(bm))) => {
                    self.bind_list.add_item(bm.clone());
                    self.bind_editor = None;
                }
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                    self.bind_editor = None;
                    return EventResult::Consumed;
                }
                _ => {}
            }
            return res;
        }

        match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.bind_editor = Some(BindMountBox::new(|bm| {
                    AppMessage::Wizard(WizardMessage::BindMountAdded(bm))
                }));

                self.bind_editor.as_mut().unwrap().set_focus(true);
                return EventResult::Consumed;
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
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.bind_mounts = self.bind_list.items().to_vec();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
