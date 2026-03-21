use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{ListItem, Paragraph},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::widgets::input::Input;
use crate::ui::widgets::list::ScrollableList;
use crate::nspawn::StatusLevel;
use crate::nspawn::storage::StorageType;
use async_trait::async_trait;

pub struct StorageStep;

impl StorageStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for StorageStep {
    fn title(&self) -> String { "Storage Backend".into() }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(1), // Title
                Constraint::Min(0),    // List
                Constraint::Length(1), // Spacer
                Constraint::Length(3), // Size input
                Constraint::Length(3), // FS input
                Constraint::Length(3), // Partition table checkbox
                Constraint::Length(2), // Hint
            ])
            .split(area);

        f.render_widget(Paragraph::new("Select storage backend for the container rootfs:"), chunks[0]);

        let mut items = Vec::new();
        for (i, (st, supported)) in context.storage_info.types.iter().enumerate() {
            let is_selected = i == context.storage_type_idx;
            
            let style = if !supported {
                Style::default().fg(Color::Rgb(60, 60, 70))
            } else if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            
            let status = if *supported { "" } else { " (unsupported on this filesystem)" };
            
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{}{}", st.label(), status), style),
            ])));
        }

        ScrollableList::new(" Storage Options ", items)
            .selected(if context.storage_field == 0 { Some(context.storage_type_idx) } else { None })
            .render(f, chunks[1]);

        let (st, supported) = context.storage_info.types[context.storage_type_idx];
        let is_raw = st == StorageType::Raw;

        if is_raw {
            let size_text = if context.storage_field == 1 { format!("{}_", context.raw_size) } else { context.raw_size.clone() };
            Input::new(" Raw Image Size (e.g. 2G, 500M) ", &size_text)
                .focused(context.storage_field == 1)
                .render(f, chunks[3]);

            let fs_text = if context.storage_field == 2 { format!("{}_", context.raw_fs) } else { context.raw_fs.clone() };
            Input::new(" Filesystem Type (ext4, xfs, btrfs, etc.) ", &fs_text)
                .focused(context.storage_field == 2)
                .render(f, chunks[4]);

            let part_text = "[ ] Create custom partition table (Advanced - Unselectable)";
            let part_style = if context.storage_field == 3 { Color::Cyan } else { Color::DarkGray };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(part_text, Style::default().fg(part_style))
                ])).block(
                    ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL)
                        .border_style(Style::default().fg(part_style))
                ),
                chunks[5],
            );
        }

        let path_hint = format!("Path: {}", st.get_path(&context.name).display());
        let mut hint_lines = vec![Line::from(Span::styled(path_hint, Style::default().fg(Color::DarkGray)))];
        
        if !supported {
            hint_lines.push(Line::from(Span::styled(" Error: This filesystem does not support this storage type! ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
        } else {
            hint_lines.push(Line::from(Span::styled(" [↑/↓] navigate, [Tab] switch field, [Enter] next, [Esc] back ", Style::default().fg(Color::Yellow))));
        }

        f.render_widget(Paragraph::new(hint_lines), chunks[6]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        let count = context.storage_info.types.len();
        let is_raw = context.storage_info.types[context.storage_type_idx].0 == StorageType::Raw;

        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Up => {
                if context.storage_field == 0 {
                    if context.storage_type_idx > 0 { context.storage_type_idx -= 1; }
                } else {
                    context.storage_field -= 1;
                }
                StepAction::None
            }
            KeyCode::Down => {
                if context.storage_field == 0 {
                    if context.storage_type_idx + 1 < count { context.storage_type_idx += 1; }
                } else {
                    let max_field = if is_raw { 3 } else { 0 };
                    if context.storage_field < max_field { context.storage_field += 1; }
                }
                StepAction::None
            }
            KeyCode::Tab => {
                let max_field = if is_raw { 3 } else { 0 };
                context.storage_field = (context.storage_field + 1) % (max_field + 1);
                StepAction::None
            }
            KeyCode::Backspace => {
                if is_raw {
                    match context.storage_field {
                        1 => { context.raw_size.pop(); }
                        2 => { context.raw_fs.pop(); }
                        _ => {}
                    }
                }
                StepAction::None
            }
            KeyCode::Char(' ') | KeyCode::Enter if context.storage_field == 3 => {
                StepAction::Status("Feature in progress: Custom partitioning is coming soon!".into(), StatusLevel::Info)
            }
            KeyCode::Char(c) => {
                if is_raw {
                    match context.storage_field {
                        1 => { context.raw_size.push(c); }
                        2 => { context.raw_fs.push(c); }
                        _ => {}
                    }
                }
                StepAction::None
            }
            KeyCode::Enter => {
                if !context.storage_info.types[context.storage_type_idx].1 {
                     return StepAction::Status("This storage backend is not supported by your filesystem".into(), StatusLevel::Error);
                }
                if is_raw && context.storage_field < 3 {
                    context.storage_field += 1;
                    return StepAction::None;
                }
                context.storage_field = 0;
                StepAction::Next
            }
            _ => StepAction::None,
        }
    }
}
