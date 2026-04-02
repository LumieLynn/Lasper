use crate::nspawn::models::{NetworkMode, PortForward};
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::ui::widgets::composites::editable_list::EditableList;
use crate::ui::widgets::composites::port_mapping::PortMappingBox;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crate::ui::wizard::context::{NetworkConfig, WizardContext};
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::Frame;

macro_rules! active_comps {
    ($self:ident) => {{
        let mode = $self.mode_selector.selected_idx();
        let is_custom = $self.is_custom_bridge();
        let mut visible: Vec<&mut dyn Component> = vec![&mut $self.mode_selector];
        if mode == 3 {
            visible.push(&mut $self.bridge_list);
            if is_custom {
                visible.push(&mut $self.custom_bridge);
            }
        }
        if mode != 0 {
            visible.push(&mut $self.port_list);
        }
        visible
    }};
}

pub struct NetworkStepView {
    mode_selector: RadioGroup,
    bridge_list: SelectableList<String>,
    custom_bridge: TextBox,
    port_list: EditableList<PortForward>,
    port_editor: Option<PortMappingBox>,
    focus: FocusTracker,
    bridge_options_len: usize,
}

impl NetworkStepView {
    pub fn new(initial_data: &NetworkConfig, scanned_bridges: &[String]) -> Self {
        let modes = vec!["Host".into(), "None".into(), "Veth".into(), "Bridge".into()];
        let mode_idx = match &initial_data.mode {
            Some(NetworkMode::Host) => 0,
            Some(NetworkMode::None) => 1,
            Some(NetworkMode::Veth) => 2,
            Some(NetworkMode::Bridge(_)) => 3,
            _ => 0,
        };

        let mut bridges = scanned_bridges.to_vec();
        bridges.push(" >> Custom Bridge... ".into());
        let bridge_options_len = bridges.len();

        let initial_bridge = match &initial_data.mode {
            Some(NetworkMode::Bridge(name)) => name.clone(),
            _ => String::new(),
        };

        let is_custom = !initial_bridge.is_empty() && !scanned_bridges.contains(&initial_bridge);
        let bridge_idx = if is_custom {
            bridges.len() - 1
        } else {
            scanned_bridges
                .iter()
                .position(|b| b == &initial_bridge)
                .unwrap_or(0)
        };

        let mut bridge_list = SelectableList::new(" Select Bridge ", bridges, |s| s.clone());
        bridge_list.select(bridge_idx);

        let mut view = Self {
            mode_selector: RadioGroup::new(" Network Mode ", modes, mode_idx),
            bridge_list,
            custom_bridge: TextBox::new(" Custom Bridge Name ", initial_bridge.clone())
                .with_validator(|v| {
                    if v.trim().is_empty() {
                        Err("Bridge name required".into())
                    } else {
                        Ok(())
                    }
                }),
            port_list: EditableList::new(
                " Configured Port Forwards ",
                initial_data.port_forwards.clone(),
                |p| format!("  {}:{}/{}", p.host, p.container, p.proto),
                |idx| AppMessage::Wizard(WizardMessage::PortForwardRemoved(idx)),
            ),

            port_editor: None,
            focus: FocusTracker::new(),
            bridge_options_len,
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

    fn update_focus(&mut self) {
        let mut visible = active_comps!(self);
        if self.focus.active_idx >= visible.len() {
            self.focus.active_idx = visible.len().saturating_sub(1);
        }
        self.focus.update_focus(&mut visible, true);
    }

    fn next(&mut self) {
        let mut visible = active_comps!(self);
        self.focus.next(&mut visible);
        self.update_focus();
    }

    fn prev(&mut self) {
        let mut visible = active_comps!(self);
        self.focus.prev(&mut visible);
        self.update_focus();
    }
}

impl Component for NetworkStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        if let Some(editor) = &mut self.port_editor {
            let inner_area = crate::ui::centered_rect(60, 60, f.area());
            f.render_widget(ratatui::widgets::Clear, inner_area);
            let block = ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title(" Add Port Forward ");
            let editor_area = block.inner(inner_area);
            f.render_widget(block, inner_area);
            editor.render(f, editor_area);
            return;
        }

        let mode = self.mode_selector.selected_idx();
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .margin(1)
            .constraints([
                ratatui::layout::Constraint::Length(3), // Mode
                ratatui::layout::Constraint::Min(5),    // Bridge/Ports
                ratatui::layout::Constraint::Length(1), // Hint
            ])
            .split(area);

        self.mode_selector.render(f, chunks[0]);

        if mode == 3 {
            let mid_chunks = ratatui::layout::Layout::default()
                .constraints([
                    ratatui::layout::Constraint::Percentage(50),
                    ratatui::layout::Constraint::Percentage(50),
                ])
                .direction(ratatui::layout::Direction::Horizontal)
                .split(chunks[1]);
            self.bridge_list.render(f, mid_chunks[0]);
            if self.is_custom_bridge() {
                let right_chunks = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints([
                        ratatui::layout::Constraint::Length(3),
                        ratatui::layout::Constraint::Min(0),
                    ])
                    .split(mid_chunks[1]);
                self.custom_bridge.render(f, right_chunks[0]);
                self.port_list.render(f, right_chunks[1]);
            } else {
                self.port_list.render(f, mid_chunks[1]);
            }
        } else if mode != 0 {
            self.port_list.render(f, chunks[1]);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if let Some(editor) = &mut self.port_editor {
            if key.code == KeyCode::Esc {
                self.port_editor = None;
                return EventResult::Consumed;
            }
            let res = editor.handle_key(key);
            match &res {
                EventResult::Message(AppMessage::Wizard(WizardMessage::PortForwardAdded(map))) => {
                    self.port_list.add_item(map.clone());
                    self.port_editor = None;
                    self.update_focus();
                }
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => {
                    self.port_editor = None;
                    self.update_focus();
                    return EventResult::Consumed;
                }
                _ => {}
            }
            return res;
        }

        match key.code {
            KeyCode::Tab => {
                self.next();
                return EventResult::Consumed;
            }
            KeyCode::BackTab => {
                self.prev();
                return EventResult::Consumed;
            }
            KeyCode::Char('a') | KeyCode::Char('A') if self.port_list.is_focused() => {
                self.port_editor = Some(PortMappingBox::new(|p| {
                    AppMessage::Wizard(WizardMessage::PortForwardAdded(p))
                }));

                self.port_editor.as_mut().unwrap().set_focus(true);
                return EventResult::Consumed;
            }
            _ => {}
        }

        let mode = self.mode_selector.selected_idx();
        let mut visible = active_comps!(self);

        if self.focus.active_idx < visible.len() {
            let res = visible[self.focus.active_idx].handle_key(key);
            if let EventResult::Consumed = res {
                if self.focus.active_idx == 0 || (mode == 3 && self.focus.active_idx == 1) {
                    self.update_focus();
                }
            }
            match &res {
                EventResult::Message(AppMessage::Wizard(WizardMessage::PortForwardRemoved(_))) => {
                    self.update_focus();
                }

                EventResult::FocusNext => {
                    self.next();
                    return EventResult::Consumed;
                }
                EventResult::FocusPrev => {
                    self.prev();
                    return EventResult::Consumed;
                }
                _ => {}
            }
            res
        } else {
            EventResult::Ignored
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if focused {
            self.update_focus();
        } else {
            self.mode_selector.set_focus(false);
            self.bridge_list.set_focus(false);
            self.custom_bridge.set_focus(false);
            self.port_list.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.mode_selector.is_focused()
            || self.bridge_list.is_focused()
            || self.custom_bridge.is_focused()
            || self.port_list.is_focused()
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.mode_selector.selected_idx() == 3 && self.is_custom_bridge() {
            self.custom_bridge.validate()?;
        }
        Ok(())
    }
}

impl StepComponent for NetworkStepView {
    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.network.mode = self.mode_selector.selected_idx();
        if self.is_custom_bridge() {
            ctx.network.bridge_name = self.custom_bridge.value().to_string();
        } else {
            ctx.network.bridge_name = self
                .bridge_list
                .selected_item()
                .cloned()
                .unwrap_or_default();
        }
        ctx.network.port_list = self.port_list.items().to_vec();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
