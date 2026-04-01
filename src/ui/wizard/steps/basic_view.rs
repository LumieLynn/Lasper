use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::composites::form::FormContainer;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::wizard::context::BasicConfig;
use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

pub struct BasicStepView {
    form: FormContainer,
}

impl BasicStepView {
    pub fn new(initial_data: &BasicConfig) -> Self {
        Self {
            form: FormContainer::new(vec![
                Box::new(
                    TextBox::new(" Container name (required) ", initial_data.name.clone())
                        .with_validator(|v| {
                            if v.trim().is_empty() {
                                Err("Name cannot be empty".to_string())
                            } else {
                                Ok(())
                            }
                        })
                        .with_on_change(|v| AppMessage::BaseNameUpdated(v)),
                ),
                Box::new(
                    TextBox::new(
                        " Hostname (optional, defaults to name) ",
                        initial_data.hostname.clone(),
                    )
                    .with_validator(|v| {
                        let s = v.trim();
                        if s.is_empty() { return Ok(()); }
                        if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
                            return Err("Invalid hostname characters".into());
                        }
                        Ok(())
                    })
                    .with_on_change(|v| AppMessage::BaseHostnameUpdated(v)),
                ),
            ]),
        }
    }
}

impl Component for BasicStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Use a centered smaller area for the form if desired, or just pass sub-area
        self.form.render(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.form.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }

    fn validate(&mut self) -> Result<(), String> {
        self.form.validate()
    }
}
