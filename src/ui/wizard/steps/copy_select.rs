use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{ListItem},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::list::ScrollableList;
use async_trait::async_trait;

pub struct CopySelectStep;

impl CopySelectStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for CopySelectStep {
    fn title(&self) -> String { "Select Container to Clone".into() }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let items: Vec<ListItem> = context.entries.iter().map(|e| {
            let (icon, color) = if e.state.is_running() { ("● ", Color::Green) } else { ("○ ", Color::DarkGray) };
            ListItem::new(Line::from(vec![
                Span::styled(icon, Style::default().fg(color)),
                Span::styled(e.name.as_str(), Style::default().fg(Color::White)),
            ]))
        }).collect();

        ScrollableList::new(" Select container to clone ", items)
            .selected(Some(context.copy_cursor))
            .render(f, chunks[0]);

        render_hint(f, chunks[1], &["[↑/↓] navigate", "[Enter] clone", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        let count = context.entries.len();
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Up => {
                if context.copy_cursor > 0 { context.copy_cursor -= 1; }
                StepAction::None
            }
            KeyCode::Down => {
                if count > 0 { context.copy_cursor = (context.copy_cursor + 1).min(count - 1); }
                StepAction::None
            }
            KeyCode::Enter => {
                if let Some(entry) = context.entries.get(context.copy_cursor) {
                    context.clone_source = entry.name.clone();
                    context.name = format!("{}-clone", entry.name);
                    StepAction::Next
                } else {
                    StepAction::None
                }
            }
            _ => StepAction::None,
        }
    }
}
