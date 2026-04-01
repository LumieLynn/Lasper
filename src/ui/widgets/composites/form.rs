use crate::ui::core::{Component, FocusTracker, EventResult};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use crossterm::event::{KeyEvent, KeyCode};

pub struct FormContainer {
    children: Vec<Box<dyn Component>>,
    focus: FocusTracker,
    height_hints: Vec<u16>,
}

impl FormContainer {
    pub fn new(children: Vec<Box<dyn Component>>) -> Self {
        let mut form = Self { 
            focus: FocusTracker::new(),
            children, 
            height_hints: vec![3; 100], // Default height 3 for most inputs, capped at 100 children
        };
        form.update_focus();
        form
    }

    pub fn with_height_hints(mut self, hints: Vec<u16>) -> Self {
        self.height_hints = hints;
        self
    }

    fn update_focus(&mut self) {
        let parent_focused = true; // FormContainer is usually focused when active
        for (i, child) in self.children.iter_mut().enumerate() {
            child.set_focus(parent_focused && i == self.focus.active_idx && child.is_focusable());
        }
    }

    fn next(&mut self) {
        let refs: Vec<&dyn Component> = self.children.iter().map(|b| b.as_ref()).collect();
        self.focus.next(&refs);
        self.update_focus();
    }

    fn prev(&mut self) {
        let refs: Vec<&dyn Component> = self.children.iter().map(|b| b.as_ref()).collect();
        self.focus.prev(&refs);
        self.update_focus();
    }
}

impl Component for FormContainer {
    fn render(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.children.is_empty() { return; }
        
        if area.height < self.children.len() as u16 * 2 {
            let msg = format!("Terminal too small! Requires at least {} height (currently {})", self.children.len() * 2, area.height);
            f.render_widget(ratatui::widgets::Paragraph::new(msg).style(ratatui::style::Style::default().fg(ratatui::style::Color::Red)), area);
            return;
        }
        
        let constraints: Vec<Constraint> = self.children.iter().enumerate()
            .map(|(i, _)| Constraint::Length(*self.height_hints.get(i).unwrap_or(&3)))
            .collect();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        for (i, child) in self.children.iter_mut().enumerate() {
            if i < chunks.len() {
                child.render(f, chunks[i]);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.children.is_empty() { return EventResult::Ignored; }

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

        let res = self.children[self.focus.active_idx].handle_key(key);
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
    
    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            for child in &mut self.children { 
                child.set_focus(false); 
            }
        }
    }

    fn is_focused(&self) -> bool {
        self.children.iter().any(|c| c.is_focused())
    }

    fn validate(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for child in &mut self.children {
            if let Err(e) = child.validate() {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        if let Some(e) = first_error {
            Err(e)
        } else {
            Ok(())
        }
    }
}
