use crate::ui::core::{AppMessage, Component, EventResult};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::selectors::selectable_list::SelectableList;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::broadcast;

pub struct DeployStepView {
    log_rx: Option<broadcast::Receiver<String>>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
    status_block: TextBlock,
    log_list: SelectableList<String>,
    internal_logs: Vec<String>,
}

impl DeployStepView {
    pub fn new(
        log_tx: broadcast::Sender<String>,
        done: Arc<AtomicBool>,
        success: Arc<AtomicBool>,
    ) -> Self {
        let rx = log_tx.subscribe();
        Self {
            log_rx: Some(rx),
            done,
            success,
            status_block: TextBlock::new(" Status ", "Deploying...".to_string()),
            log_list: SelectableList::new(" Deployment logs ", vec![], |s| s.clone()),
            internal_logs: vec![],
        }
    }

    fn update_logs(&mut self) {
        let mut changed = false;
        if let Some(rx) = &mut self.log_rx {
            loop {
                match rx.try_recv() {
                    Ok(log) => {
                        self.internal_logs.push(log);
                        changed = true;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        self.internal_logs
                            .push(format!("[{} logs skipped due to lag]", n));
                        changed = true;
                    }
                    Err(_) => break, // Empty or Closed
                }
            }
        }
        if changed {
            self.log_list.set_items(self.internal_logs.clone());
            self.log_list.select_last();
        }
    }
}

impl Component for DeployStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let done = self.done.load(Ordering::SeqCst);
        let success = self.success.load(Ordering::SeqCst);

        let status = if !done {
            "Deploying... please wait.".to_string()
        } else if success {
            "SUCCESS: Container deployed and started!".to_string()
        } else {
            "FAILED: Deployment encountered an error.".to_string()
        };
        self.status_block.set_content(status);

        self.update_logs();

        self.status_block.render(f, chunks[0]);
        self.log_list.render(f, chunks[1]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let done = self.done.load(Ordering::SeqCst);
        if !done {
            // Block all close attempts while deploying; allow log scrolling only
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    return EventResult::Consumed; // refuse silently
                }
                _ => return self.log_list.handle_key(key),
            }
        }
        // Deployment finished — allow closing
        match key.code {
            KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => {
                EventResult::Message(AppMessage::Close)
            }
            _ => self.log_list.handle_key(key),
        }
    }
}
