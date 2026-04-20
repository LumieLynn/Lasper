use crate::ui::core::{Component, EventResult};
use crate::ui::widgets::inputs::text_input_base::TextInputBase;
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct PathBox {
    base: TextInputBase,
    validator: Option<Box<dyn Fn(&str) -> Result<(), String>>>,
}

impl PathBox {
    pub fn new(label: impl Into<String>, initial_value: String) -> Self {
        Self {
            base: TextInputBase::new(label, initial_value),
            validator: None,
        }
    }

    #[allow(dead_code)]
    pub fn set_value(&mut self, value: String) {
        self.base.input = tui_input::Input::from(value);
    }

    pub fn with_validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    pub fn value(&self) -> &str {
        self.base.input.value()
    }
}

impl Component for PathBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.base.render_base(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.base.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.base.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.base.focused
    }

    fn is_enabled(&self) -> bool {
        self.base.enabled
    }

    fn validate(&mut self) -> Result<(), String> {
        let val = self.base.input.value().to_string();
        if let Some(validator) = &self.validator {
            if let Err(e) = validator(&val) {
                self.base.error_msg = Some(e.clone());
                return Err(e);
            }
        }
        self.base.error_msg = None;
        Ok(())
    }
}
