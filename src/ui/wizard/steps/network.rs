use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{ListItem, Paragraph},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::ui::widgets::list::ScrollableList;
use crate::nspawn::StatusLevel;
use async_trait::async_trait;

pub struct NetworkStep;

impl NetworkStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for NetworkStep {
    fn title(&self) -> String { "Network".into() }

    fn next_step(&self, _context: &WizardContext) -> Option<Box<dyn IStep>> {
        Some(Box::new(crate::ui::wizard::steps::passthrough::PassthroughStep::new()))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {

        let modes = ["Host", "None", "Veth", "Bridge"];
        let is_mode_focused = context.network.field_block == 0;
        let mode_spans: Vec<Span> = modes.iter().enumerate().map(|(i, m)| {
            if i == context.network.mode {
                let style = if is_mode_focused {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                };
                Span::styled(format!(" [{}] ", m), style)
            } else {
                Span::styled(format!("  {}  ", m), Style::default().fg(Color::DarkGray))
            }
        }).collect();

        let mut constraints = vec![
            Constraint::Length(1), // mode selector
            Constraint::Length(1), // hint
        ];
        if context.network.mode == 3 {
            constraints.push(Constraint::Length(3)); // bridge name
        }
        if context.network.mode != 0 {
            constraints.push(Constraint::Length(3)); // port input
            constraints.push(Constraint::Min(0));    // port list
        }
        constraints.push(Constraint::Length(1)); // final hint

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        f.render_widget(Paragraph::new(Line::from(mode_spans)), chunks[idx]);
        idx += 1;

        let desc = match context.network.mode {
            0 => "Shared host network (Warning: Container can modify host interfaces)",
            1 => "Private network namespace (lo only)",
            2 => "Virtual Ethernet pair (veth)",
            3 => "Connect to a host bridge interface",
            _ => "",
        };
        f.render_widget(Paragraph::new(format!("  Hint: {}", desc)).style(Style::default().fg(Color::DarkGray)), chunks[idx]);
        idx += 1;

        if context.network.mode == 3 {
            let is_bridge_focused = context.network.field_block == 1;
            let label = " Bridge interface (type or use ↑/↓) ";

            let display_text = if context.network.bridge_list.is_empty() {
                 if is_bridge_focused { format!("{}_", context.network.bridge_name) } else { context.network.bridge_name.clone() }
            } else {
                 if is_bridge_focused { format!("< {} >", context.network.bridge_name) } else { context.network.bridge_name.clone() }
            };

            Input::new(label, &display_text)
                .focused(is_bridge_focused)
                .render(f, chunks[idx]);
            idx += 1;
        }

        if context.network.mode != 0 {
            let is_input_focused = context.network.field_block == 2;
            let port_text = if is_input_focused { format!("{}_", context.network.port_input) } else { context.network.port_input.clone() };
            Input::new(" Port forward: host:container [/udp] — [F5]=add ", &port_text)
                .focused(is_input_focused)
                .render(f, chunks[idx]);
            idx += 1;

            let is_list_focused = context.network.field_block == 3;
            let list_width = (chunks[idx].width as usize).saturating_sub(4);

            let port_items: Vec<ListItem> = context.network.port_list.iter().enumerate().map(|(i, pf)| {
                let raw_s = format!("{}:{}/{}", pf.host, pf.container, pf.proto);
                let display_s = if is_list_focused && context.passthrough.device_cursor == i {
                    raw_s.chars().skip(context.network.h_scroll).take(list_width).collect::<String>()
                } else {
                    raw_s.chars().take(list_width).collect::<String>()
                };
                ListItem::new(Span::raw(format!("  {}", display_s)))
            }).collect();
            
            ScrollableList::new(" Configured ports — [Del/BS] remove ", port_items)
                .selected(if is_list_focused { Some(context.passthrough.device_cursor) } else { None })
                .render(f, chunks[idx]);
            idx += 1;
        }

        render_hint(f, chunks[idx], &["[Tab] cycle", "[←/→] scroll/mode", "[↑/↓] v-scroll", "[F5] add", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Left => {
                if context.network.field_block == 0 {
                    if context.network.mode > 0 { context.network.mode -= 1; }
                    if context.network.mode != 0 { context.passthrough.wayland_socket = false; }
                } else {
                    if context.network.h_scroll > 0 { context.network.h_scroll -= 1; }
                }
                StepAction::None
            }
            KeyCode::Right => {
                if context.network.field_block == 0 {
                    if context.network.mode < 3 { context.network.mode += 1; }
                    if context.network.mode != 0 { context.passthrough.wayland_socket = false; }
                } else {
                    context.network.h_scroll += 1;
                }
                StepAction::None
            }
            KeyCode::Up => {
                if context.network.field_block == 1 && context.network.bridge_cursor > 0 {
                    context.network.bridge_cursor -= 1;
                    if let Some(b) = context.network.bridge_list.get(context.network.bridge_cursor) { context.network.bridge_name = b.clone(); }
                } else if context.network.field_block == 3 && context.passthrough.device_cursor > 0 {
                    context.passthrough.device_cursor -= 1;
                    context.network.h_scroll = 0;
                }
                StepAction::None
            }
            KeyCode::Down => {
                if context.network.field_block == 1 && !context.network.bridge_list.is_empty() {
                    context.network.bridge_cursor = (context.network.bridge_cursor + 1).min(context.network.bridge_list.len() - 1);
                    if let Some(b) = context.network.bridge_list.get(context.network.bridge_cursor) { context.network.bridge_name = b.clone(); }
                } else if context.network.field_block == 3 && context.passthrough.device_cursor + 1 < context.network.port_list.len() {
                    context.passthrough.device_cursor += 1;
                    context.network.h_scroll = 0;
                }
                StepAction::None
            }
            KeyCode::Tab => {
                context.network.field_block = (context.network.field_block + 1) % 4;
                if context.network.mode < 3 && context.network.field_block == 1 { context.network.field_block = 2; }
                if context.network.mode == 0 && context.network.field_block >= 2 { context.network.field_block = 0; }
                context.passthrough.device_cursor = 0;
                context.network.h_scroll = 0;
                StepAction::None
            }
            KeyCode::BackTab => {
                context.network.field_block = if context.network.field_block == 0 { 3 } else { context.network.field_block - 1 };
                if context.network.mode == 0 && context.network.field_block >= 2 { context.network.field_block = 0; }
                if context.network.mode < 3 && context.network.field_block == 1 { context.network.field_block = 0; }
                context.passthrough.device_cursor = 0;
                context.network.h_scroll = 0;
                StepAction::None
            }
            KeyCode::F(5) => {
                let input = context.network.port_input.trim().to_string();
                if let Some(pf) = WizardContext::parse_port(&input) {
                    context.network.port_list.push(pf);
                    context.network.port_input.clear();
                    StepAction::None
                } else {
                    StepAction::Status("Format: host:container [/udp]".into(), StatusLevel::Error)
                }
            }
            KeyCode::Delete | KeyCode::Backspace => {
                if context.network.field_block == 1 { if context.network.bridge_list.is_empty() { context.network.bridge_name.pop(); } }
                else if context.network.field_block == 2 { context.network.port_input.pop(); }
                else if context.network.field_block == 3 {
                    if !context.network.port_list.is_empty() && context.passthrough.device_cursor < context.network.port_list.len() {
                        context.network.port_list.remove(context.passthrough.device_cursor);
                        if context.passthrough.device_cursor >= context.network.port_list.len() && !context.network.port_list.is_empty() {
                            context.passthrough.device_cursor = context.network.port_list.len() - 1;
                        }
                    }
                }
                StepAction::None
            }
            KeyCode::Char(c) => {
                if context.network.field_block == 1 { if context.network.bridge_list.is_empty() { context.network.bridge_name.push(c); } }
                else if context.network.field_block == 2 { context.network.port_input.push(c); }
                StepAction::None
            }
            KeyCode::Enter => { context.network.field_block = 0; StepAction::Next }
            _ => StepAction::None,
        }
    }
}
