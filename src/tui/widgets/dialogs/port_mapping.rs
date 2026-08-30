use crate::domain::provisioning::PortForward;
use crate::tui::core::{AppMessage, Component, FocusTracker, WizardMessage};

use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::number_box::NumberBox;
use crate::tui::widgets::selectors::radio_group::RadioGroup;

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
            btn_ok: Button::new("OK", || AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", || AppMessage::Wizard(WizardMessage::DialogCancel)),

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

form_dialog!(
    PortMappingBox,
    " Add Port Forward ",
    (30, 40),
    [host_port, container_port, protocol]
);
