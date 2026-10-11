use crate::domain::provisioning::{validate_network_interface_name, NetworkMode, PortForward};
use crate::tui::features::wizard::draft::{NetworkConfig, WizardDraft};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::features::wizard::StepAction;
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::inputs::text_input::{Input, TextInput};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::foundation::widgets::selectors::radio::{RadioControl, RadioState};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    Frame,
};

const MODES: &[&str] = &[
    "Host", "None", "Veth", "Bridge", "MacVlan", "IpVlan", "Physical",
];
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetworkControl {
    Mode,
    Bridges,
    CustomBridge,
    Interfaces,
    CustomInterface,
    Ports,
}
pub struct NetworkStepView {
    mode: RadioState,
    bridges: Vec<String>,
    bridge_selection: ListControlState,
    custom_bridge: Input,
    bridge_error: Option<String>,
    interfaces: Vec<String>,
    interface_selection: ListControlState,
    custom_interface: Input,
    interface_error: Option<String>,
    ports: Vec<PortForward>,
    port_selection: ListControlState,
    form: FormState<NetworkControl>,
}
impl NetworkStepView {
    pub fn new(
        initial: &NetworkConfig,
        scanned_bridges: &[String],
        scanned_interfaces: &[String],
    ) -> Self {
        let mode = match &initial.mode {
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
        let bridge = match &initial.mode {
            Some(NetworkMode::Bridge(name)) => name.clone(),
            _ => String::new(),
        };
        let bridge_index = initial_option_index(scanned_bridges, &bridge);
        let mut interfaces = scanned_interfaces.to_vec();
        interfaces.push(" >> Custom Interface... ".into());
        let interface = match &initial.mode {
            Some(NetworkMode::MacVlan(name))
            | Some(NetworkMode::IpVlan(name))
            | Some(NetworkMode::Interface(name)) => name.clone(),
            _ => String::new(),
        };
        let interface_index = initial_option_index(scanned_interfaces, &interface);
        let mut view = Self {
            mode: RadioState::new(mode),
            bridges,
            bridge_selection: ListControlState::new(bridge_index),
            custom_bridge: Input::from(bridge),
            bridge_error: None,
            interfaces,
            interface_selection: ListControlState::new(interface_index),
            custom_interface: Input::from(interface),
            interface_error: None,
            ports: initial.port_forwards.clone(),
            port_selection: ListControlState::new(0),
            form: FormState::new(NetworkControl::Mode),
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }
    fn is_custom_bridge(&self) -> bool {
        self.bridge_selection.selected() == Some(self.bridges.len() - 1)
    }
    fn is_custom_interface(&self) -> bool {
        self.interface_selection.selected() == Some(self.interfaces.len() - 1)
    }
    fn supports_port_forwarding(&self) -> bool {
        matches!(self.mode.selected_idx(), 2 | 3)
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, NetworkControl>) -> R) -> R {
        let mode_index = self.mode.selected_idx();
        let custom_bridge = self.is_custom_bridge();
        let custom_interface = self.is_custom_interface();
        let bridge_entries: Vec<_> = self
            .bridges
            .iter()
            .map(|name| ListEntry::new(name.as_str()))
            .collect();
        let interface_entries: Vec<_> = self
            .interfaces
            .iter()
            .map(|name| ListEntry::new(name.as_str()))
            .collect();
        let port_entries: Vec<_> = self
            .ports
            .iter()
            .map(|port| {
                ListEntry::new(format!("  {}:{}/{}", port.host, port.container, port.proto))
            })
            .collect();
        let mut mode = RadioControl::new(&mut self.mode, " Network Mode ", MODES);
        let mut bridges = ListControl::new(
            &mut self.bridge_selection,
            " Select Bridge ",
            &bridge_entries,
        );
        let mut interfaces = ListControl::new(
            &mut self.interface_selection,
            " Select Interface ",
            &interface_entries,
        );
        let mut bridge = TextInput::new(
            &mut self.custom_bridge,
            " Custom Bridge Name ",
            self.bridge_error.as_deref(),
        );
        let mut interface = TextInput::new(
            &mut self.custom_interface,
            " Custom Interface Name ",
            self.interface_error.as_deref(),
        );
        let mut ports = ListControl::new(
            &mut self.port_selection,
            " Configured Port Forwards ",
            &port_entries,
        );
        let mut items = vec![FormItem::field(NetworkControl::Mode, &mut mode)];
        match mode_index {
            3 => {
                let mut right = Vec::new();
                if custom_bridge {
                    right.push(FormItem::field(NetworkControl::CustomBridge, &mut bridge));
                }
                right.push(FormItem::field(NetworkControl::Ports, &mut ports).fill(4));
                items.push(
                    FormItem::row([
                        FormItem::field(NetworkControl::Bridges, &mut bridges).fill(4),
                        FormItem::column(right),
                    ])
                    .fill(5),
                );
            }
            4.. => {
                let right = if custom_interface {
                    FormItem::column([
                        FormItem::field(NetworkControl::CustomInterface, &mut interface),
                        FormItem::spacer(),
                    ])
                } else {
                    FormItem::spacer()
                };
                items.push(
                    FormItem::row([
                        FormItem::field(NetworkControl::Interfaces, &mut interfaces).fill(4),
                        right,
                    ])
                    .fill(5),
                );
            }
            2 => items.push(FormItem::field(NetworkControl::Ports, &mut ports).fill(5)),
            _ => items.push(FormItem::spacer()),
        }
        visit(&mut Form::new(&mut self.form, items))
    }
}
fn initial_option_index(options: &[String], value: &str) -> usize {
    if !value.is_empty() && !options.iter().any(|option| option == value) {
        options.len()
    } else {
        options
            .iter()
            .position(|option| option == value)
            .unwrap_or(0)
    }
}
impl Component for NetworkStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
        if self.form.is_selected(NetworkControl::Ports) {
            footer.register(FooterHint::new("a", "add port").on_key(KeyCode::Char('a')));
            if self.port_selection.selected().is_some() {
                footer.register(FooterHint::new("e", "edit port").on_key(KeyCode::Char('e')));
                footer.register(FooterHint::new("d", "delete").on_key(KeyCode::Char('d')));
            }
        }
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.with_form(|form| form.render(frame, area.inner(Margin::new(1, 1))));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.form.is_selected(NetworkControl::Ports) {
            match key.code {
                KeyCode::Char('a' | 'A') => {
                    return EventResult::Message(AppMessage::Wizard(WizardMessage::OpenPortDialog))
                }
                KeyCode::Char('e' | 'E') => {
                    if let Some(index) = self.port_selection.selected() {
                        if let Some(port) = self.ports.get(index) {
                            return EventResult::Message(AppMessage::Wizard(
                                WizardMessage::OpenPortEditDialog(index, port.clone()),
                            ));
                        }
                    }
                }
                KeyCode::Delete | KeyCode::Char('d' | 'D') => {
                    if let Some(index) = self
                        .port_selection
                        .selected()
                        .filter(|index| *index < self.ports.len())
                    {
                        self.ports.remove(index);
                        self.port_selection.removed(index, self.ports.len());
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::PortForwardRemoved(index),
                        ));
                    }
                }
                _ => {}
            }
        }
        field_event(self.with_form(|form| form.handle_key(key)))
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        field_event(self.with_form(|form| form.handle_mouse(mouse)))
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
    fn validate(&mut self) -> Result<(), String> {
        self.bridge_error = if self.mode.selected_idx() == 3 && self.is_custom_bridge() {
            validate_network_interface_name(self.custom_bridge.value())
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        self.interface_error = if self.mode.selected_idx() >= 4 && self.is_custom_interface() {
            validate_network_interface_name(self.custom_interface.value())
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        if let Some(error) = self.bridge_error.as_ref().or(self.interface_error.as_ref()) {
            Err(error.clone())
        } else {
            Ok(())
        }
    }
}
impl StepComponent for NetworkStepView {
    fn handle_message(&mut self, message: &AppMessage) -> StepAction {
        match message {
            AppMessage::Wizard(WizardMessage::PortForwardAdded(port)) => {
                self.ports.push(port.clone());
                self.with_form(|_| ());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::PortForwardUpdated(index, port)) => {
                if let Some(current) = self.ports.get_mut(*index) {
                    *current = port.clone();
                }
                self.with_form(|_| ());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => StepAction::CloseDialog,
            _ => StepAction::None,
        }
    }
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        let mode = self.mode.selected_idx();
        context.network.mode = mode;
        if mode == 3 {
            context.network.bridge_name = if self.is_custom_bridge() {
                self.custom_bridge.value().into()
            } else {
                self.bridge_selection
                    .selected()
                    .and_then(|index| self.bridges.get(index))
                    .cloned()
                    .unwrap_or_default()
            };
        } else if mode >= 4 {
            context.network.interface_name = if self.is_custom_interface() {
                self.custom_interface.value().into()
            } else {
                self.interface_selection
                    .selected()
                    .and_then(|index| self.interfaces.get(index))
                    .cloned()
                    .unwrap_or_default()
            };
        }
        context.network.port_list = if self.supports_port_forwarding() {
            self.ports.clone()
        } else {
            Vec::new()
        };
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}
