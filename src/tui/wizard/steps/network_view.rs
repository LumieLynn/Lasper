use crate::domain::provisioning::{NetworkMode, PortForward};
use crate::tui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::lists::editable_list::EditableList;
use crate::tui::widgets::lists::selectable_list::SelectableList;
use crate::tui::widgets::selectors::radio_group::RadioGroup;
use crate::tui::wizard::draft::{NetworkConfig, WizardDraft};
use crate::tui::wizard::steps::StepComponent;
use crate::tui::wizard::StepAction;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::Paragraph,
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let mode = $self.mode_selector.selected_idx();
        let is_custom_bridge = $self.is_custom_bridge();
        let is_custom_interface = $self.is_custom_interface();
        let mut visible: Vec<&mut dyn Component> = vec![&mut $self.mode_selector];
        if mode == 3 {
            visible.push(&mut $self.bridge_list);
            if is_custom_bridge {
                visible.push(&mut $self.custom_bridge);
            }
        } else if mode >= 4 {
            visible.push(&mut $self.interface_list);
            if is_custom_interface {
                visible.push(&mut $self.custom_interface);
            }
        }
        if mode == 2 || mode == 3 {
            visible.push(&mut $self.port_list);
        }
        visible
    }};
}

impl_wizard_nav!(NetworkStepView, active_comps);

pub struct NetworkStepView {
    mode_selector: RadioGroup,
    bridge_list: SelectableList<String>,
    custom_bridge: TextBox,
    interface_list: SelectableList<String>,
    custom_interface: TextBox,
    port_list: EditableList<PortForward>,
    focus: FocusTracker,
    bridge_options_len: usize,
    interface_options_len: usize,
}

impl NetworkStepView {
    pub fn new(
        initial_data: &NetworkConfig,
        scanned_bridges: &[String],
        scanned_interfaces: &[String],
    ) -> Self {
        let modes = vec![
            "Host".into(),
            "None".into(),
            "Veth".into(),
            "Bridge".into(),
            "MacVlan".into(),
            "IpVlan".into(),
            "Physical".into(),
        ];
        let mode_idx = match &initial_data.mode {
            Some(NetworkMode::Host) => 0,
            Some(NetworkMode::None) => 1,
            Some(NetworkMode::Veth) => 2,
            Some(NetworkMode::Bridge(_)) => 3,
            Some(NetworkMode::MacVlan(_)) => 4,
            Some(NetworkMode::IpVlan(_)) => 5,
            Some(NetworkMode::Interface(_)) => 6,
            _ => 0,
        };

        let mut bridges = scanned_bridges.to_vec();
        bridges.push(" >> Custom Bridge... ".into());
        let bridge_options_len = bridges.len();

        let initial_bridge = match &initial_data.mode {
            Some(NetworkMode::Bridge(name)) => name.clone(),
            _ => String::new(),
        };

        let is_custom_bridge =
            !initial_bridge.is_empty() && !scanned_bridges.contains(&initial_bridge);
        let bridge_idx = if is_custom_bridge {
            bridges.len() - 1
        } else {
            scanned_bridges
                .iter()
                .position(|b| b == &initial_bridge)
                .unwrap_or(0)
        };

        let mut bridge_list = SelectableList::new(" Select Bridge ", bridges, |s| s.clone());
        bridge_list.select(bridge_idx);

        let mut interfaces = scanned_interfaces.to_vec();
        interfaces.push(" >> Custom Interface... ".into());
        let interface_options_len = interfaces.len();

        let initial_interface = match &initial_data.mode {
            Some(NetworkMode::MacVlan(name))
            | Some(NetworkMode::IpVlan(name))
            | Some(NetworkMode::Interface(name)) => name.clone(),
            _ => String::new(),
        };

        let is_custom_iface =
            !initial_interface.is_empty() && !scanned_interfaces.contains(&initial_interface);
        let interface_idx = if is_custom_iface {
            interfaces.len() - 1
        } else {
            scanned_interfaces
                .iter()
                .position(|i| i == &initial_interface)
                .unwrap_or(0)
        };

        let mut interface_list =
            SelectableList::new(" Select Interface ", interfaces, |s| s.clone());
        interface_list.select(interface_idx);

        let mut view = Self {
            mode_selector: RadioGroup::new(" Network Mode ", modes, mode_idx),
            bridge_list,
            custom_bridge: TextBox::new(" Custom Bridge Name ", initial_bridge.clone())
                .with_validator(|value| {
                    crate::domain::provisioning::validate_network_interface_name(value)
                        .map_err(|error| error.to_string())
                }),
            interface_list,
            custom_interface: TextBox::new(" Custom Interface Name ", initial_interface.clone())
                .with_validator(|value| {
                    crate::domain::provisioning::validate_network_interface_name(value)
                        .map_err(|error| error.to_string())
                }),
            port_list: EditableList::new(
                " Configured Port Forwards ",
                initial_data.port_forwards.clone(),
                |p| format!("  {}:{}/{}", p.host, p.container, p.proto),
                |idx| AppMessage::Wizard(WizardMessage::PortForwardRemoved(idx)),
            ),

            focus: FocusTracker::new(),
            bridge_options_len,
            interface_options_len,
        };
        view.update_focus();
        view
    }

    // pub fn with_port_editor(mut self, enabled: bool) -> Self {
    //     if enabled {
    //         self.port_editor = Some(PortMappingBox::new(|p| {
    //             AppMessage::Wizard(WizardMessage::PortForwardAdded(p))
    //         }));

    //         if let Some(ref mut editor) = self.port_editor {
    //             editor.set_focus(true);
    //         }
    //     } else {
    //         self.port_editor = None;
    //     }
    //     self
    // }

    fn is_custom_bridge(&self) -> bool {
        self.bridge_list.selected_idx() == Some(self.bridge_options_len - 1)
    }

    fn is_custom_interface(&self) -> bool {
        self.interface_list.selected_idx() == Some(self.interface_options_len - 1)
    }

    fn supports_port_forwarding(&self) -> bool {
        matches!(self.mode_selector.selected_idx(), 2 | 3)
    }
}

impl Component for NetworkStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let mode = self.mode_selector.selected_idx();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Mode
                Constraint::Min(5),    // Bridge/Ports
                Constraint::Length(1), // Hint
            ])
            .split(area);

        self.mode_selector.render(f, chunks[0]);

        if mode == 3 {
            let mid_chunks = Layout::default()
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .direction(Direction::Horizontal)
                .split(chunks[1]);
            self.bridge_list.render(f, mid_chunks[0]);
            if self.is_custom_bridge() {
                let right_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(0)])
                    .split(mid_chunks[1]);
                self.custom_bridge.render(f, right_chunks[0]);
                self.port_list.render(f, right_chunks[1]);
            } else {
                self.port_list.render(f, mid_chunks[1]);
            }
        } else if mode >= 4 {
            let mid_chunks = Layout::default()
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .direction(Direction::Horizontal)
                .split(chunks[1]);
            self.interface_list.render(f, mid_chunks[0]);
            if self.is_custom_interface() {
                let right_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(0)])
                    .split(mid_chunks[1]);
                self.custom_interface.render(f, right_chunks[0]);
            }
        } else if mode == 2 {
            self.port_list.render(f, chunks[1]);
        }

        let hint = if mode == 2 || mode == 3 {
            " [A]dd port, [E]dit port, [D]elete port "
        } else {
            ""
        };
        if !hint.is_empty() {
            f.render_widget(
                Paragraph::new(hint)
                    .style(Style::default().fg(crate::tui::theme::theme().wizard_footer)),
                chunks[2],
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.port_list.is_focused() {
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    return EventResult::Message(AppMessage::Wizard(WizardMessage::OpenPortDialog));
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    if let Some(pf) = self.port_list.selected_item() {
                        let idx = self.port_list.selected();
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::OpenPortEditDialog(idx, pf.clone()),
                        ));
                    }
                }
                _ => {}
            }
        }

        let mode = self.mode_selector.selected_idx();
        let res = delegate_wizard_navigation!(self, key, active_comps);

        if let EventResult::Consumed = res {
            if self.focus.active_idx == 0
                || (mode == 3 && self.focus.active_idx == 1)
                || (mode >= 4 && self.focus.active_idx == 1)
            {
                self.update_focus();
            }
        }
        if let EventResult::Message(AppMessage::Wizard(WizardMessage::PortForwardRemoved(_))) = res
        {
            self.update_focus();
        }
        res
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.mode_selector.selected_idx() == 3 && self.is_custom_bridge() {
            self.custom_bridge.validate()?;
        }
        if self.mode_selector.selected_idx() >= 4 && self.is_custom_interface() {
            self.custom_interface.validate()?;
        }
        Ok(())
    }
}

impl StepComponent for NetworkStepView {
    fn handle_message(&mut self, msg: &AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(WizardMessage::PortForwardAdded(pf)) => {
                self.port_list.add_item(pf.clone());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::PortForwardUpdated(idx, pf)) => {
                self.port_list.update_item(*idx, pf.clone());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => StepAction::CloseDialog,
            _ => StepAction::None,
        }
    }

    fn commit_to_draft(&self, ctx: &mut WizardDraft) {
        ctx.network.mode = self.mode_selector.selected_idx();
        if self.mode_selector.selected_idx() == 3 {
            if self.is_custom_bridge() {
                ctx.network.bridge_name = self.custom_bridge.value().to_string();
            } else {
                ctx.network.bridge_name = self
                    .bridge_list
                    .selected_item()
                    .cloned()
                    .unwrap_or_default();
            }
        } else if self.mode_selector.selected_idx() >= 4 {
            if self.is_custom_interface() {
                ctx.network.interface_name = self.custom_interface.value().to_string();
            } else {
                ctx.network.interface_name = self
                    .interface_list
                    .selected_item()
                    .cloned()
                    .unwrap_or_default();
            }
        }
        ctx.network.port_list = if self.supports_port_forwarding() {
            self.port_list.items().to_vec()
        } else {
            Vec::new()
        };
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }
}
