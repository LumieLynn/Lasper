use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::inputs::text_input_base::TextInputBase;
use tui_input::backend::crossterm::EventHandler;

pub struct NumberBox {
    base: TextInputBase,
    min_value: u32,
    max_value: u32,
    on_change: Option<Box<dyn Fn(u32) -> AppMessage>>,
}

impl NumberBox {
    pub fn new(label: impl Into<String>, initial_value: u32) -> Self {
        Self {
            base: TextInputBase::new(label, initial_value.to_string()),
            min_value: 0,
            max_value: u32::MAX,
            on_change: None,
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.base.enabled = enabled;
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.base.enabled = enabled;
    }

    pub fn with_on_change<F>(mut self, f: F) -> Self 
    where F: Fn(u32) -> AppMessage + 'static 
    {
        self.on_change = Some(Box::new(f));
        self
    }

    pub fn with_max_value(mut self, max: u32) -> Self {
        self.max_value = max;
        self
    }
    
    pub fn with_min_value(mut self, min: u32) -> Self {
        self.min_value = min;
        self
    }

    pub fn value(&self) -> u32 {
        self.base.input.value().parse().unwrap_or(0)
    }
}

impl Component for NumberBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.base.render_base(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let mut temp_input = self.base.input.clone();
        
        let res = self.base.handle_key(key);
        if let EventResult::FocusNext | EventResult::FocusPrev = res {
            return res;
        }

        if let Some(_) = temp_input.handle_event(&crossterm::event::Event::Key(key)) {
            let val_str = temp_input.value();
            
            if val_str.is_empty() {
                self.base.input = temp_input;
                return EventResult::Consumed;
            }
            
            if let Ok(num) = val_str.parse::<u32>() {
                if num <= self.max_value {
                    self.base.input = temp_input;
                    if let Some(on_change) = &self.on_change {
                        return EventResult::Message(on_change(num));
                    }
                    return EventResult::Consumed;
                }
            }
            return EventResult::Consumed;
        }
        EventResult::Ignored
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
        self.base.error_msg = None;
        let val_str = self.base.input.value();
        if val_str.is_empty() {
            let msg = "Cannot be empty".to_string();
            self.base.error_msg = Some(msg.clone());
            return Err(msg);
        }
        if let Ok(num) = val_str.parse::<u32>() {
            if num > self.max_value {
                let msg = format!("Max value is {}", self.max_value);
                self.base.error_msg = Some(msg.clone());
                return Err(msg);
            }
            if num < self.min_value {
                let msg = format!("Min value is {}", self.min_value);
                self.base.error_msg = Some(msg.clone());
                return Err(msg);
            }
        } else {
            let msg = "Invalid number".to_string();
            self.base.error_msg = Some(msg.clone());
            return Err(msg);
        }
        Ok(())
    }
}
