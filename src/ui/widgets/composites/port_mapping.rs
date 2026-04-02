use crate::nspawn::models::PortForward;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::number_box::NumberBox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

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

    fn update_focus(&mut self) {
        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.host_port,
            &mut self.container_port,
            &mut self.protocol,
        ];
        self.focus.update_focus(&mut comps, true);
        let mut components: Vec<&mut dyn Component> = vec![
            &mut self.host_port,
            &mut self.container_port,
            &mut self.protocol,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];
        self.focus.update_focus(&mut components, true);
    }
}

impl Component for PortMappingBox {
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
                let comps: Vec<&dyn Component> = vec![
                    &self.host_port,
                    &self.container_port,
                    &self.protocol,
                    &self.btn_ok,
                    &self.btn_cancel,
                ];
                self.focus.next(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                let comps: Vec<&dyn Component> = vec![
                    &self.host_port,
                    &self.container_port,
                    &self.protocol,
                    &self.btn_ok,
                    &self.btn_cancel,
                ];
                self.focus.prev(&comps);
                self.update_focus();
                return EventResult::Consumed;
            }
            KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                let mut valid = true;
                if self.host_port.validate().is_err() {
                    valid = false;
                }
                if self.container_port.validate().is_err() {
                    valid = false;
                }
                if !valid {
                    return EventResult::Consumed;
                }

                let host = self.host_port.value() as u16;
                let container = self.container_port.value() as u16;
                let proto = match self.protocol.selected_idx() {
                    0 => "tcp".to_string(),
                    1 => "udp".to_string(),
                    _ => "tcp".to_string(),
                };

                return EventResult::Message((self.on_submit)(PortForward {
                    host,
                    container,
                    proto,
                }));
            }
            _ => {}
        }

        let mut comps: Vec<&mut dyn Component> = vec![
            &mut self.host_port,
            &mut self.container_port,
            &mut self.protocol,
            &mut self.btn_ok,
            &mut self.btn_cancel,
        ];

        let res = comps[self.focus.active_idx].handle_key(key);

        match res {
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
                let mut valid = true;
                if self.host_port.validate().is_err() {
                    valid = false;
                }
                if self.container_port.validate().is_err() {
                    valid = false;
                }
                if !valid {
                    return EventResult::Consumed;
                }

                let host = self.host_port.value() as u16;
                let container = self.container_port.value() as u16;
                let proto = match self.protocol.selected_idx() {
                    0 => "tcp".to_string(),
                    1 => "udp".to_string(),
                    _ => "tcp".to_string(),
                };

                EventResult::Message((self.on_submit)(PortForward {
                    host,
                    container,
                    proto,
                }))
            }
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }

            EventResult::FocusNext => {
                let crefs: Vec<&dyn Component> = vec![
                    &self.host_port,
                    &self.container_port,
                    &self.protocol,
                    &self.btn_ok,
                    &self.btn_cancel,
                ];
                self.focus.next(&crefs);
                self.update_focus();
                EventResult::Consumed
            }
            EventResult::FocusPrev => {
                let crefs: Vec<&dyn Component> = vec![
                    &self.host_port,
                    &self.container_port,
                    &self.protocol,
                    &self.btn_ok,
                    &self.btn_cancel,
                ];
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
            self.host_port.set_focus(false);
            self.container_port.set_focus(false);
            self.protocol.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }
}
