use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker};
use crate::ui::widgets::inputs::path_box::PathBox;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::inputs::button::Button;
use crate::nspawn::models::BindMount;
use crossterm::event::{KeyEvent, KeyCode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub struct BindMountBox {
    source_path: PathBox,
    target_path: PathBox,
    readonly: Checkbox,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(BindMount) -> AppMessage>,
}

impl BindMountBox {
    pub fn new(on_submit: impl Fn(BindMount) -> AppMessage + 'static) -> Self {
        Self {
            source_path: PathBox::new("Source Path", "/".to_string())
                .with_validator(|v| {
                    let path = std::path::Path::new(v.trim());
                    if v.trim().is_empty() { return Err("Path required".into()); }
                    if !path.is_absolute() { return Err("Must be absolute path".into()); }
                    if !path.exists() { return Err("Path does not exist".into()); }
                    Ok(())
                }),
            target_path: PathBox::new("Target Path (optional, defaults to source)", "".to_string())
                .with_validator(|v| {
                    let trimmed = v.trim();
                    if trimmed.is_empty() { return Ok(()); }
                    if !std::path::Path::new(trimmed).is_absolute() { return Err("Must be absolute path".into()); }
                    Ok(())
                }),
            readonly: Checkbox::new("Read Only", false),
            btn_ok: Button::new("OK", AppMessage::DialogSubmit),
            btn_cancel: Button::new("Cancel", AppMessage::DialogCancel),
            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        }
    }

    fn update_focus(&mut self) {
        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.source_path,
            &mut self.target_path,
            &mut self.readonly,
        ];
        self.focus.update_focus(&mut comps, true);
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.source_path,
            &mut self.target_path,
            &mut self.readonly,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];
        self.focus.update_focus(&mut components, true);
    }
}

impl Component for BindMountBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);

        self.source_path.render(f, chunks[0]);
        self.target_path.render(f, chunks[1]);
        self.readonly.render(f, chunks[2]);
        
        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(chunks[4]);
            
        let ok_area = crate::ui::centered_rect(60, 100, btn_chunks[0]);
        let cancel_area = crate::ui::centered_rect(60, 100, btn_chunks[1]);
        self.btn_ok.render(f, ok_area);
        self.btn_cancel.render(f, cancel_area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Tab => {
                let comps: Vec<&dyn Component> = vec![&self.source_path, &self.target_path, &self.readonly, &self.btn_ok, &self.btn_cancel];
                self.focus.next(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                let comps: Vec<&dyn Component> = vec![&self.source_path, &self.target_path, &self.readonly, &self.btn_ok, &self.btn_cancel];
                self.focus.prev(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                let mut valid = true;
                if self.source_path.validate().is_err() { valid = false; }
                if self.target_path.validate().is_err() { valid = false; }
                if !valid { return EventResult::Consumed; }
                
                let source = self.source_path.value().trim().to_string();
                let mut target = self.target_path.value().trim().to_string();
                if target.is_empty() {
                    target = source.clone();
                }
                let readonly = self.readonly.checked();
                
                return EventResult::Message((self.on_submit)(BindMount { source, target, readonly }));
            }
            _ => {}
        }

        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.source_path,
            &mut self.target_path,
            &mut self.readonly,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];
        
        let res = comps[self.focus.active_idx].handle_key(key);

        match res {
            EventResult::Message(AppMessage::DialogSubmit) => {
                let mut valid = true;
                if self.source_path.validate().is_err() { valid = false; }
                if self.target_path.validate().is_err() { valid = false; }
                if !valid { return EventResult::Consumed; }
                
                let source = self.source_path.value().trim().to_string();
                let mut target = self.target_path.value().trim().to_string();
                if target.is_empty() {
                    target = source.clone();
                }
                let readonly = self.readonly.checked();
                
                EventResult::Message((self.on_submit)(BindMount { source, target, readonly }))
            }
            EventResult::Message(AppMessage::DialogCancel) => {
                EventResult::Message(AppMessage::DialogCancel)
            }
            EventResult::FocusNext => {
                let crefs: Vec<&dyn Component> = vec![&self.source_path, &self.target_path, &self.readonly, &self.btn_ok, &self.btn_cancel];
                self.focus.next(&crefs);
                self.update_focus();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                let crefs: Vec<&dyn Component> = vec![&self.source_path, &self.target_path, &self.readonly, &self.btn_ok, &self.btn_cancel];
                self.focus.prev(&crefs);
                self.update_focus();
                EventResult::Consumed
            }
            _ => res,
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.source_path.set_focus(false);
            self.target_path.set_focus(false);
            self.readonly.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.source_path.is_focused() || self.target_path.is_focused() || self.readonly.is_focused()
    }
}
