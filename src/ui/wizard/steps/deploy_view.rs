use crate::nspawn::ops::provision::{DeployLogEvent, DeployProgress, DeploymentCancellation};
use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::soft_wrap_text;
use crate::ui::widgets::dialogs::confirmation::ConfirmationDialog;
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Gauge, Paragraph},
    Frame,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::broadcast;

pub struct DeployStepView {
    log_rx: Option<broadcast::Receiver<DeployLogEvent>>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    rolling_back: Arc<AtomicBool>,
    cancellation: DeploymentCancellation,
    status_block: TextBlock,
    log_list: SelectableList<String>,
    internal_logs: Vec<String>,
    progress: Option<DeployProgress>,
    cancel_dialog: Option<ConfirmationDialog>,
}

impl DeployStepView {
    pub fn new(
        log_rx: broadcast::Receiver<DeployLogEvent>,
        done: Arc<AtomicBool>,
        success: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        rolling_back: Arc<AtomicBool>,
        cancellation: DeploymentCancellation,
    ) -> Self {
        Self {
            log_rx: Some(log_rx),
            done,
            success,
            cancelled,
            rolling_back,
            cancellation,
            status_block: TextBlock::new(" Status ", "Deploying...".to_string()),
            log_list: SelectableList::new(" Deployment logs ", vec![], |s| s.clone()),
            internal_logs: vec![],
            progress: None,
            cancel_dialog: None,
        }
    }

    fn update_logs(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &mut self.log_rx {
            loop {
                match rx.try_recv() {
                    Ok(DeployLogEvent::Line(log)) => {
                        self.progress = None;
                        self.internal_logs.push(log);
                        changed = true;
                    }
                    Ok(DeployLogEvent::Progress(progress)) => {
                        self.progress = Some(progress);
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
        changed
    }
}

fn wrap_log_lines(logs: &[String], max_width: usize) -> Vec<String> {
    logs.iter()
        .flat_map(|line| soft_wrap_text(line, max_width))
        .collect()
}

impl Component for DeployStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let done = self.done.load(Ordering::SeqCst);
        let success = self.success.load(Ordering::SeqCst);
        let cancelled = self.cancelled.load(Ordering::SeqCst);
        let rolling_back = self.rolling_back.load(Ordering::SeqCst);
        if done {
            self.cancel_dialog = None;
        }

        let status = if rolling_back {
            "Rolling back deployment changes...".to_string()
        } else if !done && self.cancellation.is_requested() {
            "Cancellation requested; waiting for a safe rollback point...".to_string()
        } else if !done {
            "Deploying... please wait.".to_string()
        } else if success {
            "SUCCESS: Deployment completed.".to_string()
        } else if cancelled {
            "CANCELLED: Deployment stopped; review rollback logs.".to_string()
        } else {
            "FAILED: Deployment encountered an error.".to_string()
        };
        self.status_block.set_content(status);

        let logs_changed = self.update_logs();
        let was_following_tail = self
            .log_list
            .selected_idx()
            .is_some_and(|selected| selected + 1 == self.log_list.items().len());
        let log_width = usize::from(chunks[1].width.saturating_sub(5).max(1));
        self.log_list
            .set_items(wrap_log_lines(&self.internal_logs, log_width));
        if logs_changed || was_following_tail {
            self.log_list.select_last();
        }

        if let Some(progress) = self.progress.as_ref().filter(|_| !done) {
            let ratio = f64::from(progress.permille) / 1000.0;
            let label = format!("{}.{:01}%", progress.permille / 10, progress.permille % 10);
            let theme = crate::ui::theme::theme();
            let gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(format!(" {} ", progress.label)),
                )
                .gauge_style(Style::default().fg(theme.accent))
                .ratio(ratio)
                .label(label);
            f.render_widget(gauge, chunks[0]);
        } else {
            self.status_block.render(f, chunks[0]);
        }
        self.log_list.render(f, chunks[1]);

        let hint = if done {
            " [Enter/q/Esc] Close "
        } else if self.cancellation.is_requested() {
            " Cancellation in progress "
        } else {
            " [q/Esc] Cancel deployment "
        };
        f.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(crate::ui::theme::theme().wizard_footer)),
            chunks[2],
        );

        if let Some(dialog) = &mut self.cancel_dialog {
            dialog.render(f, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let done = self.done.load(Ordering::SeqCst);
        if done {
            self.cancel_dialog = None;
            return match key.code {
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => {
                    EventResult::Message(AppMessage::Wizard(WizardMessage::Close))
                }
                _ => self.log_list.handle_key(key),
            };
        }

        if let Some(dialog) = &mut self.cancel_dialog {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.cancel_dialog = None;
                    self.cancellation.request();
                    EventResult::Consumed
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.cancel_dialog = None;
                    EventResult::Consumed
                }
                _ => {
                    let _ = dialog.handle_key(key);
                    EventResult::Consumed
                }
            };
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc if !self.cancellation.is_requested() => {
                self.cancel_dialog = Some(ConfirmationDialog::new(
                    "Cancel Deployment?",
                    "Stop this deployment and roll back changes created by it?",
                ));
                EventResult::Consumed
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => EventResult::Consumed,
            _ => self.log_list.handle_key(key),
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for DeployStepView {
    fn commit_to_context(&self, _ctx: &mut WizardContext) {
        // Deploy is terminal state
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_log_lines_soft_wrap_by_display_width() {
        assert_eq!(
            wrap_log_lines(&["abcdefghij".into()], 8),
            ["abcdefgh", "ij"]
        );
        assert_eq!(
            wrap_log_lines(
                &["\u{5bb9}\u{5668}\u{4e0b}\u{8f7d}\u{5b8c}\u{6210}".into()],
                9
            ),
            ["\u{5bb9}\u{5668}\u{4e0b}\u{8f7d}", "\u{5b8c}\u{6210}"]
        );
        assert_eq!(
            wrap_log_lines(&["fatal error with a long explanation".into()], 12),
            ["fatal error", "with a long", "explanation"]
        );
    }

    #[test]
    fn cancellation_requires_confirmation() {
        let (tx, rx) = broadcast::channel(4);
        let cancellation = DeploymentCancellation::default();
        let mut view = DeployStepView::new(
            rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            cancellation.clone(),
        );
        drop(tx);

        let result = view.handle_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(result, EventResult::Consumed);
        assert!(view.cancel_dialog.is_some());
        assert!(!cancellation.is_requested());

        let result = view.handle_key(KeyEvent::new(
            KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(result, EventResult::Consumed);
        assert!(view.cancel_dialog.is_none());
        assert!(cancellation.is_requested());
    }

    #[test]
    fn declining_cancellation_keeps_deployment_running() {
        let (tx, rx) = broadcast::channel(4);
        let cancellation = DeploymentCancellation::default();
        let mut view = DeployStepView::new(
            rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            cancellation.clone(),
        );
        drop(tx);

        let _ = view.handle_key(KeyEvent::new(
            KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let result = view.handle_key(KeyEvent::new(
            KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(result, EventResult::Consumed);
        assert!(view.cancel_dialog.is_none());
        assert!(!cancellation.is_requested());
    }
}
