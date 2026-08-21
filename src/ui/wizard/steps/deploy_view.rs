use crate::application::provisioning::{
    DeploymentEvent, DeploymentJobHandle, DeploymentProgress, DeploymentStatus,
};
use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::soft_wrap_text;
use crate::ui::widgets::dialogs::confirmation::ConfirmationDialog;
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::wizard::draft::WizardDraft;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Gauge, Paragraph},
    Frame,
};
pub struct DeployStepView {
    job: DeploymentJobHandle,
    status_block: TextBlock,
    log_list: SelectableList<String>,
    internal_logs: Vec<String>,
    progress: Option<DeploymentProgress>,
    cancel_dialog: Option<ConfirmationDialog>,
}

impl DeployStepView {
    pub fn new(job: DeploymentJobHandle) -> Self {
        Self {
            job,
            status_block: TextBlock::new(" Status ", "Deploying...".to_string()),
            log_list: SelectableList::new(" Deployment logs ", vec![], |s| s.clone()),
            internal_logs: vec![],
            progress: None,
            cancel_dialog: None,
        }
    }

    fn update_logs(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.job.try_recv() {
                Ok(DeploymentEvent::Line(log)) => {
                    self.progress = None;
                    self.internal_logs.push(log);
                    changed = true;
                }
                Ok(DeploymentEvent::Progress(progress)) => {
                    self.progress = Some(progress);
                    changed = true;
                }
                Err(_) => break,
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

        let job_status = self.job.status();
        let done = job_status.is_finished();
        if done {
            self.cancel_dialog = None;
        }

        let status = if job_status == DeploymentStatus::RollingBack {
            "Rolling back deployment changes...".to_string()
        } else if !done && self.job.cancellation_requested() {
            "Cancellation requested; waiting for a safe rollback point...".to_string()
        } else {
            match &job_status {
                DeploymentStatus::Running => "Deploying... please wait.".to_string(),
                DeploymentStatus::RollingBack => unreachable!(),
                DeploymentStatus::Succeeded => "SUCCESS: Deployment completed.".to_string(),
                DeploymentStatus::Cancelled(message) => format!("CANCELLED: {message}"),
                DeploymentStatus::Failed(message) => format!("FAILED: {message}"),
            }
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
        } else if self.job.cancellation_requested() {
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
        let done = self.job.status().is_finished();
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
                    self.job.request_cancel();
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
            KeyCode::Char('q') | KeyCode::Esc if !self.job.cancellation_requested() => {
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
    fn commit_to_draft(&self, _ctx: &mut WizardDraft) {
        // Deploy is terminal state
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{deployment_job_channel, DeploymentId};

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
        let (handle, context) = deployment_job_channel(DeploymentId::new(1).unwrap());
        let cancellation = context.cancellation();
        let mut view = DeployStepView::new(handle);

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
        let (handle, context) = deployment_job_channel(DeploymentId::new(1).unwrap());
        let cancellation = context.cancellation();
        let mut view = DeployStepView::new(handle);

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
