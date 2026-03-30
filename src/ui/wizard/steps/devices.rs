use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Span,
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

pub struct DevicesStep;

impl DevicesStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for DevicesStep {
    fn title(&self) -> String { "Devices & Bind Mounts".into() }

    fn next_step(&self, _context: &WizardContext) -> Option<Box<dyn IStep>> {
        Some(Box::new(crate::ui::wizard::steps::review::ReviewStep::new()))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(10), // NVIDIA sections
                Constraint::Length(1),
                Constraint::Length(3),  // Bind input
                Constraint::Min(0),     // Bind list
                Constraint::Length(1),  // Hint
            ])
            .split(area);

        if context.passthrough.nvidia_enabled {
            f.render_widget(Paragraph::new("\n  NVIDIA GPU Passthrough is ENABLED.\n  (Lasper will automatically manage devices and libraries via JIT assembly)").style(Style::default().fg(Color::Cyan)), chunks[0]);
        } else {
            f.render_widget(Paragraph::new("\n  NVIDIA passthrough is disabled. Skip to bind mounts below."), chunks[0]);
        }

        let is_input = context.passthrough.device_block == 3;
        let bind_text = if is_input { format!("{}_", context.passthrough.bind_input) } else { context.passthrough.bind_input.clone() };
        Input::new(" Custom Bind Mount: host_path:container_path[:ro] — [F5]=add ", &bind_text)
            .focused(is_input)
            .render(f, chunks[2]);

        let is_list = context.passthrough.device_block == 4;
        let list_width = (chunks[3].width as usize).saturating_sub(4);

        let bind_items: Vec<ListItem> = context.passthrough.bind_list.iter().enumerate().map(|(i, bm)| {
            let raw_s = format!("{}:{} ({})", bm.source, bm.target, if bm.readonly { "ro" } else { "rw" });
            let display_s = if is_list && context.passthrough.device_cursor == i {
                raw_s.chars().skip(context.passthrough.device_h_scroll).take(list_width).collect::<String>()
            } else {
                raw_s.chars().take(list_width).collect::<String>()
            };
            ListItem::new(Span::raw(format!("  {}", display_s)))
        }).collect();

        ScrollableList::new(" Configured mounts — [Del/BS] remove ", bind_items)
            .selected(if is_list { Some(context.passthrough.device_cursor) } else { None })
            .render(f, chunks[3]);

        render_hint(f, chunks[4], &["[Tab] cycle", "[←/→] h-scroll", "[↑/↓] v-scroll", "[Space] toggle", "[F5] add", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Tab => {
                context.passthrough.device_block = if context.passthrough.device_block == 3 { 4 } else { 3 };
                context.passthrough.device_cursor = 0;
                context.passthrough.device_h_scroll = 0;
                StepAction::None
            }
            KeyCode::BackTab => {
                context.passthrough.device_block = if context.passthrough.device_block == 3 { 4 } else { 3 };
                context.passthrough.device_cursor = 0;
                context.passthrough.device_h_scroll = 0;
                StepAction::None
            }
            KeyCode::Left => {
                if context.passthrough.device_h_scroll > 0 { context.passthrough.device_h_scroll -= 1; }
                StepAction::None
            }
            KeyCode::Right => {
                context.passthrough.device_h_scroll += 1;
                StepAction::None
            }
            KeyCode::Up => {
                if context.passthrough.device_cursor > 0 { context.passthrough.device_cursor -= 1; context.passthrough.device_h_scroll = 0; }
                StepAction::None
            }
            KeyCode::Down => {
                let max = if context.passthrough.device_block == 4 { context.passthrough.bind_list.len() } else { 0 };
                if context.passthrough.device_cursor + 1 < max { context.passthrough.device_cursor += 1; context.passthrough.device_h_scroll = 0; }
                StepAction::None
            }
            KeyCode::Char(' ') => StepAction::None,
            KeyCode::F(5) => {
                if let Some(bm) = WizardContext::parse_bind_mount(&context.passthrough.bind_input) {
                    context.passthrough.bind_list.push(bm);
                    context.passthrough.bind_input.clear();
                    StepAction::None
                } else {
                    StepAction::Status("Format: host:container[:ro]".into(), StatusLevel::Error)
                }
            }
            KeyCode::Delete | KeyCode::Backspace => {
                if context.passthrough.device_block == 3 {
                    context.passthrough.bind_input.pop();
                } else if context.passthrough.device_block == 4 {
                    if !context.passthrough.bind_list.is_empty() && context.passthrough.device_cursor < context.passthrough.bind_list.len() {
                        context.passthrough.bind_list.remove(context.passthrough.device_cursor);
                        if context.passthrough.device_cursor >= context.passthrough.bind_list.len() && !context.passthrough.bind_list.is_empty() {
                            context.passthrough.device_cursor = context.passthrough.bind_list.len() - 1;
                        }
                    }
                }
                StepAction::None
            }
            KeyCode::Char(c) => {
                if context.passthrough.device_block == 3 { context.passthrough.bind_input.push(c); }
                StepAction::None
            }
            KeyCode::Enter => {
                let cp = context.build_config();
                context.review.preview = cp.preview;
                context.review.preview_scroll = 0;
                StepAction::Next
            }
            _ => StepAction::None,
        }
    }
}
