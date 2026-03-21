use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
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

        if context.nvidia_enabled {
            let constraints = match context.device_block {
                0 => [Constraint::Percentage(45), Constraint::Percentage(27), Constraint::Percentage(28)],
                1 => [Constraint::Percentage(27), Constraint::Percentage(45), Constraint::Percentage(28)],
                2 => [Constraint::Percentage(27), Constraint::Percentage(28), Constraint::Percentage(45)],
                _ => [Constraint::Percentage(33), Constraint::Percentage(33), Constraint::Percentage(34)],
            };

            let nv_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(chunks[0]);

            let render_nv_list = |f: &mut Frame, area: Rect, title: &str, items: &Vec<String>, sel: &Vec<bool>, block_idx: usize| {
                let is_focused = context.device_block == block_idx;
                let list_width = (area.width as usize).saturating_sub(6);

                let list_items: Vec<ListItem> = items.iter().enumerate().map(|(i, s)| {
                    let is_sel = sel.get(i).copied().unwrap_or(false);
                    let prefix = if is_sel { "[x] " } else { "[ ] " };
                    let display_s = if is_focused && context.device_cursor == i {
                        s.chars().skip(context.device_h_scroll).take(list_width).collect::<String>()
                    } else {
                        s.chars().take(list_width).collect::<String>()
                    };
                    ListItem::new(Span::raw(format!("{}{}", prefix, display_s)))
                }).collect();

                ScrollableList::new(title, list_items)
                    .selected(if is_focused { Some(context.device_cursor) } else { None })
                    .render(f, area);
            };

            render_nv_list(f, nv_chunks[0], " GPU Devices ", &context.nvidia.devices, &context.nvidia_devices_sel, 0);
            render_nv_list(f, nv_chunks[1], " System RO ", &context.nvidia.system_ro, &context.nvidia_sysro_sel, 1);
            render_nv_list(f, nv_chunks[2], " Driver Libs ", &context.nvidia.driver_files, &context.nvidia_libs_sel, 2);
        } else {
            f.render_widget(Paragraph::new("\n  NVIDIA passthrough is disabled. Skip to bind mounts below."), chunks[0]);
        }

        let is_input = context.device_block == 3;
        let bind_text = if is_input { format!("{}_", context.bind_input) } else { context.bind_input.clone() };
        Input::new(" Custom Bind Mount: host_path:container_path[:ro] — [F5]=add ", &bind_text)
            .focused(is_input)
            .render(f, chunks[2]);

        let is_list = context.device_block == 4;
        let list_width = (chunks[3].width as usize).saturating_sub(4);

        let bind_items: Vec<ListItem> = context.bind_list.iter().enumerate().map(|(i, bm)| {
            let raw_s = format!("{}:{} ({})", bm.source, bm.target, if bm.readonly { "ro" } else { "rw" });
            let display_s = if is_list && context.device_cursor == i {
                raw_s.chars().skip(context.device_h_scroll).take(list_width).collect::<String>()
            } else {
                raw_s.chars().take(list_width).collect::<String>()
            };
            ListItem::new(Span::raw(format!("  {}", display_s)))
        }).collect();

        ScrollableList::new(" Configured mounts — [Del/BS] remove ", bind_items)
            .selected(if is_list { Some(context.device_cursor) } else { None })
            .render(f, chunks[3]);

        render_hint(f, chunks[4], &["[Tab] cycle", "[←/→] h-scroll", "[↑/↓] v-scroll", "[Space] toggle", "[F5] add", "[Enter] next", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Tab => {
                context.device_block = (context.device_block + 1) % 5;
                if !context.nvidia_enabled && context.device_block < 3 { context.device_block = 3; }
                context.device_cursor = 0;
                context.device_h_scroll = 0;
                StepAction::None
            }
            KeyCode::BackTab => {
                context.device_block = if context.device_block == 0 { 4 } else { context.device_block - 1 };
                if !context.nvidia_enabled && context.device_block < 3 { context.device_block = 4; }
                context.device_cursor = 0;
                context.device_h_scroll = 0;
                StepAction::None
            }
            KeyCode::Left => {
                if context.device_h_scroll > 0 { context.device_h_scroll -= 1; }
                StepAction::None
            }
            KeyCode::Right => {
                context.device_h_scroll += 1;
                StepAction::None
            }
            KeyCode::Up => {
                if context.device_cursor > 0 { context.device_cursor -= 1; context.device_h_scroll = 0; }
                StepAction::None
            }
            KeyCode::Down => {
                let max = match context.device_block {
                    0 => context.nvidia.devices.len(),
                    1 => context.nvidia.system_ro.len(),
                    2 => context.nvidia.driver_files.len(),
                    4 => context.bind_list.len(),
                    _ => 0,
                };
                if context.device_cursor + 1 < max { context.device_cursor += 1; context.device_h_scroll = 0; }
                StepAction::None
            }
            KeyCode::Char(' ') => {
                match context.device_block {
                    0 => if let Some(b) = context.nvidia_devices_sel.get_mut(context.device_cursor) { *b = !*b; }
                    1 => if let Some(b) = context.nvidia_sysro_sel.get_mut(context.device_cursor) { *b = !*b; }
                    2 => if let Some(b) = context.nvidia_libs_sel.get_mut(context.device_cursor) { *b = !*b; }
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::F(5) => {
                if let Some(bm) = WizardContext::parse_bind_mount(&context.bind_input) {
                    context.bind_list.push(bm);
                    context.bind_input.clear();
                    StepAction::None
                } else {
                    StepAction::Status("Format: host:container[:ro]".into(), StatusLevel::Error)
                }
            }
            KeyCode::Delete | KeyCode::Backspace => {
                if context.device_block == 3 {
                    context.bind_input.pop();
                } else if context.device_block == 4 {
                    if !context.bind_list.is_empty() && context.device_cursor < context.bind_list.len() {
                        context.bind_list.remove(context.device_cursor);
                        if context.device_cursor >= context.bind_list.len() && !context.bind_list.is_empty() {
                            context.device_cursor = context.bind_list.len() - 1;
                        }
                    }
                }
                StepAction::None
            }
            KeyCode::Char(c) => {
                if context.device_block == 3 { context.bind_input.push(c); }
                StepAction::None
            }
            KeyCode::Enter => {
                let cp = context.build_config();
                context.preview = cp.preview;
                context.preview_scroll = 0;
                StepAction::Next
            }
            _ => StepAction::None,
        }
    }
}
