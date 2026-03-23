use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{ListItem},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::ui::widgets::list::ScrollableList;
use std::sync::atomic::Ordering;
use async_trait::async_trait;

pub struct DeployStep;

impl DeployStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for DeployStep {
    fn title(&self) -> String { "Deploying Container".into() }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let done = context.deploy.done.load(Ordering::SeqCst);
        let success = context.deploy.success.load(Ordering::SeqCst);

        let (status_text, _status_color) = if !done {
            ("Deploying... please wait.", Color::Yellow)
        } else if success {
            ("SUCCESS: Container deployed and started!", Color::Green)
        } else {
            ("FAILED: Deployment encountered an error.", Color::Red)
        };

        Input::new(" Status ", status_text)
            .focused(true)
            .render(f, chunks[0]);

        let logs = context.deploy.logs.lock().unwrap();
        let log_len = logs.len();
        
        // Auto-scroll to bottom if not done.
        let scroll = if !done && log_len > 0 {
            log_len - 1
        } else {
            context.deploy.scroll
        };

        let log_items: Vec<ListItem> = logs.iter().map(|l| {
            let color = if l.contains("ERR") || l.contains("FATAL") || l.contains("failed") { Color::Red }
                        else if l.contains("SUCCESS") || l.contains("done") || l.contains("Complete") { Color::Green }
                        else if l.starts_with("===") { Color::Cyan }
                        else { Color::DarkGray };
            ListItem::new(l.as_str()).style(Style::default().fg(color))
        }).collect();

        ScrollableList::new(" Deployment logs ", log_items)
            .selected(if log_len > 0 { Some(scroll) } else { None })
            .render(f, chunks[1]);

        if done {
            render_hint(f, chunks[2], &["[↑/↓] scroll", "[Enter/Esc] finish"][..]);
        } else {
            render_hint(f, chunks[2], &["[Tab] background (NYI)"][..]);
        }
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        let done = context.deploy.done.load(Ordering::SeqCst);
        let log_len = context.deploy.logs.lock().unwrap().len();

        match key.code {
            KeyCode::Up => {
                if done && context.deploy.scroll > 0 {
                    context.deploy.scroll -= 1;
                }
                StepAction::None
            }
            KeyCode::Down => {
                if done && log_len > 0 && context.deploy.scroll < log_len - 1 {
                    context.deploy.scroll += 1;
                }
                StepAction::None
            }
            KeyCode::Enter | KeyCode::Esc => {
                if done {
                    if context.deploy.success.load(Ordering::SeqCst) {
                        StepAction::CloseRefresh
                    } else {
                        StepAction::Close
                    }
                } else {
                    StepAction::None
                }
            }
            _ => StepAction::None,
        }
    }
}
