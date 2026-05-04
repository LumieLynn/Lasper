use crate::nspawn::models::{BindMount, IdmapSuffix};
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::path_box::PathBox;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![
            &mut $self.source_path,
            &mut $self.target_path,
            &mut $self.readonly,
            &mut $self.suffix,
            &mut $self.btn_ok,
            &mut $self.btn_cancel,
        ];
        comps
    }};
}

pub struct BindMountBox {
    source_path: PathBox,
    target_path: PathBox,
    readonly: Checkbox,
    suffix: RadioGroup,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(BindMount) -> AppMessage>,
}

impl BindMountBox {
    pub fn new(on_submit: impl Fn(BindMount) -> AppMessage + 'static) -> Self {
        Self {
            source_path: PathBox::new("Source Path", "/".to_string()).with_validator(|v| {
                let path = std::path::Path::new(v.trim());
                if v.trim().is_empty() {
                    return Err("Path required".into());
                }
                if !path.is_absolute() {
                    return Err("Must be absolute path".into());
                }
                if !path.exists() {
                    return Err("Path does not exist".into());
                }
                Ok(())
            }),
            target_path: PathBox::new("Target Path (optional, defaults to source)", "".to_string())
                .with_validator(|v| {
                    let trimmed = v.trim();
                    if trimmed.is_empty() {
                        return Ok(());
                    }
                    if !std::path::Path::new(trimmed).is_absolute() {
                        return Err("Must be absolute path".into());
                    }
                    Ok(())
                }),
            readonly: Checkbox::new("Read Only", false),
            suffix: RadioGroup::new(
                "ID Mapping",
                vec![
                    "None".to_string(),
                    "noidmap".to_string(),
                    "idmap".to_string(),
                    "rootidmap".to_string(),
                    "owneridmap".to_string(),
                ],
                0,
            ),
            btn_ok: Button::new("OK", AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", AppMessage::Wizard(WizardMessage::DialogCancel)),

            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        }
    }

    fn update_focus(&mut self) {
        let mut comps = active_comps!(self);
        self.focus.update_focus(&mut comps, true);
    }

    fn next(&mut self) {
        let comps = active_comps!(self);
        self.focus.next(&comps);
        self.update_focus();
    }

    fn prev(&mut self) {
        let comps = active_comps!(self);
        self.focus.prev(&comps);
        self.update_focus();
    }

    pub fn with_mount(mut self, bm: &BindMount) -> Self {
        self.source_path = PathBox::new("Source Path", bm.source.clone()).with_validator(|v| {
            let path = std::path::Path::new(v.trim());
            if v.trim().is_empty() {
                return Err("Path required".into());
            }
            if !path.is_absolute() {
                return Err("Must be absolute path".into());
            }
            if !path.exists() {
                return Err("Path does not exist".into());
            }
            Ok(())
        });
        self.target_path = PathBox::new("Target Path (optional)", bm.target.clone())
            .with_validator(|v| {
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    return Ok(());
                }
                if !std::path::Path::new(trimmed).is_absolute() {
                    return Err("Must be absolute path".into());
                }
                Ok(())
            });
        self.readonly = Checkbox::new("Read Only", bm.readonly);
        self.suffix = RadioGroup::new(
            "ID Mapping",
            vec![
                "None".to_string(),
                "noidmap".to_string(),
                "idmap".to_string(),
                "rootidmap".to_string(),
                "owneridmap".to_string(),
            ],
            bm.suffix.to_index(),
        );
        self.update_focus();
        self
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        let mut valid = true;
        if self.source_path.validate().is_err() {
            valid = false;
        }
        if self.target_path.validate().is_err() {
            valid = false;
        }
        if !valid {
            return None;
        }
        let source = self.source_path.value().trim().to_string();
        let mut target = self.target_path.value().trim().to_string();
        if target.is_empty() {
            target = source.clone();
        }
        Some((self.on_submit)(BindMount {
            source,
            target,
            readonly: self.readonly.checked(),
            suffix: IdmapSuffix::from_index(self.suffix.selected_idx()),
        }))
    }
}

impl Component for BindMountBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let dialog_area = crate::ui::centered_rect(45, 55, area);
        f.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add Bind Mount ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(inner);

        self.source_path.render(f, chunks[0]);
        self.target_path.render(f, chunks[1]);
        self.readonly.render(f, chunks[2]);
        self.suffix.render(f, chunks[3]);

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[5]);

        let ok_area = crate::ui::centered_rect(60, 100, btn_chunks[0]);
        let cancel_area = crate::ui::centered_rect(60, 100, btn_chunks[1]);
        self.btn_ok.render(f, ok_area);
        self.btn_cancel.render(f, cancel_area);
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
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                return if let Some(msg) = self.try_submit() {
                    EventResult::Message(msg)
                } else {
                    EventResult::Consumed
                };
            }
            _ => {}
        }

        let mut comps = active_comps!(self);
        let res = comps[self.focus.active_idx].handle_key(key);
        match res {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
                if let Some(msg) = self.try_submit() {
                    EventResult::Message(msg)
                } else {
                    EventResult::Consumed
                }
            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => res,
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
            self.source_path.set_focus(false);
            self.target_path.set_focus(false);
            self.readonly.set_focus(false);
            self.suffix.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.source_path.is_focused()
            || self.target_path.is_focused()
            || self.readonly.is_focused()
            || self.suffix.is_focused()
    }
}
