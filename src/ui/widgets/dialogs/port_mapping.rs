use crate::nspawn::models::PortForward;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::number_box::NumberBox;
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
            &mut $self.host_port,
            &mut $self.container_port,
            &mut $self.protocol,
            &mut $self.btn_ok,
            &mut $self.btn_cancel,
        ];
        comps
    }};
}

pub struct PortMappingBox {
    host_port: NumberBox,
    container_port: NumberBox,
    protocol: RadioGroup,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(PortForward) -> AppMessage>,
}

impl PortMappingBox {
    pub fn new(on_submit: impl Fn(PortForward) -> AppMessage + 'static) -> Self {
        Self {
            host_port: NumberBox::new("Host Port", 0)
                .with_max_value(65535)
                .with_min_value(1),
            container_port: NumberBox::new("Container Port", 0)
                .with_max_value(65535)
                .with_min_value(1),
            protocol: RadioGroup::new("Protocol", vec!["tcp".to_string(), "udp".to_string()], 0),
            btn_ok: Button::new("OK", AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", AppMessage::Wizard(WizardMessage::DialogCancel)),

            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        }
    }

    pub fn with_port(mut self, pf: &PortForward) -> Self {
        self.host_port = NumberBox::new("Host Port", pf.host as u32)
            .with_max_value(65535)
            .with_min_value(1);
        self.container_port = NumberBox::new("Container Port", pf.container as u32)
            .with_max_value(65535)
            .with_min_value(1);
        let proto_idx = if pf.proto == "udp" { 1 } else { 0 };
        self.protocol.set_selected_idx(proto_idx);
        self.update_focus();
        self
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

    fn try_submit(&mut self) -> Option<AppMessage> {
        let mut valid = true;
        if self.host_port.validate().is_err() {
            valid = false;
        }
        if self.container_port.validate().is_err() {
            valid = false;
        }
        if !valid {
            return None;
        }
        let proto = match self.protocol.selected_idx() {
            0 => "tcp".to_string(),
            1 => "udp".to_string(),
            _ => "tcp".to_string(),
        };
        Some((self.on_submit)(PortForward {
            host: self.host_port.value() as u16,
            container: self.container_port.value() as u16,
            proto,
        }))
    }
}

impl Component for PortMappingBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let dialog_area = crate::ui::centered_rect(30, 40, area);
        f.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add Port Forward ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(inner);

        self.host_port.render(f, chunks[0]);
        self.container_port.render(f, chunks[1]);
        self.protocol.render(f, chunks[2]);

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[4]);

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
            self.host_port.set_focus(false);
            self.container_port.set_focus(false);
            self.protocol.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }
}
