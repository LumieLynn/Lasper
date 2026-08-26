use crate::application::provisioning::{
    DeploymentClaimStatus, DeploymentEvent, DeploymentJobHandle, DeploymentProgress,
    DeploymentStatus,
};
use crate::tui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::tui::soft_wrap_text;
use crate::tui::widgets::dialogs::confirmation::ConfirmationDialog;
use crate::tui::widgets::display::text_block::TextBlock;
use crate::tui::widgets::lists::selectable_list::SelectableList;
use crate::tui::wizard::draft::WizardDraft;
use crate::tui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Gauge, Paragraph},
    Frame,
};
use std::collections::VecDeque;

const MAX_DEPLOYMENT_LOG_LINES: usize = 5000;

struct DeploymentLogs {
    lines: VecDeque<String>,
    max_lines: usize,
    wrapped_width: Option<usize>,
    dirty: bool,
}

impl DeploymentLogs {
    fn new(max_lines: usize) -> Self {
        let max_lines = max_lines.max(1);
        Self {
            lines: VecDeque::with_capacity(max_lines),
            max_lines,
            wrapped_width: None,
            dirty: true,
        }
    }

    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
        self.dirty = true;
    }

    fn wrapped_if_changed(&mut self, width: usize) -> Option<Vec<String>> {
        let width = width.max(1);
        if !self.dirty && self.wrapped_width == Some(width) {
            return None;
        }
        self.dirty = false;
        self.wrapped_width = Some(width);
        Some(wrap_log_lines(self.lines.iter(), width))
    }
}

pub struct DeployStepView {
    job: DeploymentJobHandle,
    status_block: TextBlock,
    log_list: SelectableList<String>,
    logs: DeploymentLogs,
    progress: Option<DeploymentProgress>,
    cancel_dialog: Option<ConfirmationDialog>,
    release_dialog: Option<ConfirmationDialog>,
    release_pending: bool,
}

impl DeployStepView {
    pub fn new(job: DeploymentJobHandle) -> Self {
        Self {
            job,
            status_block: TextBlock::new(" Status ", "Deploying...".to_string()),
            log_list: SelectableList::new(" Deployment logs ", vec![], |s| s.clone()),
            logs: DeploymentLogs::new(MAX_DEPLOYMENT_LOG_LINES),
            progress: None,
            cancel_dialog: None,
            release_dialog: None,
            release_pending: false,
        }
    }

    fn update_logs(&mut self) {
        loop {
            match self.job.try_recv() {
                Ok(DeploymentEvent::Line(log)) => {
                    self.progress = None;
                    self.logs.push(log);
                }
                Ok(DeploymentEvent::Progress(progress)) => {
                    self.progress = Some(progress);
                }
                Err(_) => break,
            }
        }
    }
}

fn wrap_log_lines<'a>(logs: impl IntoIterator<Item = &'a String>, max_width: usize) -> Vec<String> {
    logs.into_iter()
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
        let claim_status = self.job.claim_status();
        let done = job_status.is_finished();
        if done {
            self.cancel_dialog = None;
        }
        if claim_status != DeploymentClaimStatus::ReconciliationRequired {
            self.release_dialog = None;
            self.release_pending = false;
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
                DeploymentStatus::ReconciliationRequired(message) => match claim_status {
                    DeploymentClaimStatus::ReconciliationRequired => {
                        format!("RECONCILIATION REQUIRED: {message}")
                    }
                    DeploymentClaimStatus::Reconciled => {
                        format!("RECONCILED UNKNOWN OUTCOME: {message}")
                    }
                    DeploymentClaimStatus::ReleasedUnresolved => {
                        format!("UNRESOLVED CLAIM RELEASED: {message}")
                    }
                    DeploymentClaimStatus::Held | DeploymentClaimStatus::Released => {
                        format!("UNKNOWN DEPLOYMENT OUTCOME: {message}")
                    }
                },
            }
        };
        self.status_block.set_content(status);

        self.update_logs();
        let log_list_was_empty = self.log_list.items().is_empty();
        let was_following_tail = self
            .log_list
            .selected_idx()
            .is_some_and(|selected| selected + 1 == self.log_list.items().len());
        let log_width = usize::from(chunks[1].width.saturating_sub(5).max(1));
        if let Some(wrapped) = self.logs.wrapped_if_changed(log_width) {
            self.log_list.set_items(wrapped);
            if log_list_was_empty || was_following_tail {
                self.log_list.select_last();
            }
        }

        if let Some(progress) = self.progress.as_ref().filter(|_| !done) {
            let ratio = f64::from(progress.permille) / 1000.0;
            let label = format!("{}.{:01}%", progress.permille / 10, progress.permille % 10);
            let theme = crate::tui::theme::theme();
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

        let hint = if done && self.release_pending {
            " Releasing unresolved coordination claim... "
        } else if done && claim_status == DeploymentClaimStatus::ReconciliationRequired {
            " [r] Release unresolved claim  [Enter/q/Esc] Close "
        } else if done {
            " [Enter/q/Esc] Close "
        } else if self.job.cancellation_requested() {
            " Cancellation in progress "
        } else {
            " [q/Esc] Cancel deployment "
        };
        f.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(crate::tui::theme::theme().wizard_footer)),
            chunks[2],
        );

        if let Some(dialog) = &mut self.cancel_dialog {
            dialog.render(f, area);
        }
        if let Some(dialog) = &mut self.release_dialog {
            dialog.render(f, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let done = self.job.status().is_finished();
        if done {
            self.cancel_dialog = None;
            if let Some(dialog) = &mut self.release_dialog {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.release_dialog = None;
                        self.release_pending = true;
                        EventResult::Message(AppMessage::Wizard(
                            WizardMessage::ReleaseUnresolvedDeployment(self.job.id()),
                        ))
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        self.release_dialog = None;
                        EventResult::Consumed
                    }
                    _ => {
                        let _ = dialog.handle_key(key);
                        EventResult::Consumed
                    }
                };
            }
            return match key.code {
                KeyCode::Char('r')
                    if self.job.claim_status() == DeploymentClaimStatus::ReconciliationRequired
                        && !self.release_pending =>
                {
                    self.release_dialog = Some(ConfirmationDialog::new(
                        "Release Unresolved Claim?",
                        "Release only Lasper's coordination claim? The deployment outcome will remain unknown and durable recovery state will be kept.",
                    ));
                    EventResult::Consumed
                }
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

    fn handle_message(&mut self, msg: &AppMessage) -> crate::tui::wizard::StepAction {
        let AppMessage::Wizard(WizardMessage::UnresolvedDeploymentReleaseFinished {
            deployment_id,
            error,
        }) = msg
        else {
            return crate::tui::wizard::StepAction::None;
        };
        if *deployment_id != self.job.id() {
            return crate::tui::wizard::StepAction::None;
        }
        self.release_pending = false;
        match error {
            Some(error) => {
                self.logs.push(format!("CLAIM RELEASE FAILED: {error}"));
                crate::tui::wizard::StepAction::Status(
                    format!("Could not release unresolved deployment claim: {error}"),
                    crate::tui::StatusLevel::Error,
                )
            }
            None => crate::tui::wizard::StepAction::Status(
                "Released the unresolved coordination claim; the historical deployment outcome remains unknown."
                    .into(),
                crate::tui::StatusLevel::Warn,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{deployment_job_channel, DeploymentId};

    #[test]
    fn long_log_lines_soft_wrap_by_display_width() {
        let ascii = ["abcdefghij".to_string()];
        assert_eq!(wrap_log_lines(ascii.iter(), 8), ["abcdefgh", "ij"]);
        let wide = ["\u{5bb9}\u{5668}\u{4e0b}\u{8f7d}\u{5b8c}\u{6210}".to_string()];
        assert_eq!(
            wrap_log_lines(wide.iter(), 9),
            ["\u{5bb9}\u{5668}\u{4e0b}\u{8f7d}", "\u{5b8c}\u{6210}"]
        );
        let words = ["fatal error with a long explanation".to_string()];
        assert_eq!(
            wrap_log_lines(words.iter(), 12),
            ["fatal error", "with a long", "explanation"]
        );
    }

    #[test]
    fn deployment_logs_are_bounded_and_rewrap_only_when_needed() {
        let mut logs = DeploymentLogs::new(2);
        logs.push("first".into());
        logs.push("second".into());
        logs.push("third".into());

        assert_eq!(
            logs.lines.iter().cloned().collect::<Vec<_>>(),
            ["second", "third"]
        );
        assert_eq!(logs.wrapped_if_changed(20).unwrap(), ["second", "third"]);
        assert!(logs.wrapped_if_changed(20).is_none());
        assert_eq!(
            logs.wrapped_if_changed(3).unwrap(),
            ["sec", "ond", "thi", "rd"]
        );
    }

    #[test]
    fn cancellation_requires_confirmation() {
        let (handle, context) = deployment_job_channel(DeploymentId::from_u128(1));
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
        let (handle, context) = deployment_job_channel(DeploymentId::from_u128(1));
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

    #[test]
    fn unresolved_claim_release_has_its_own_confirmation_focus() {
        let (handle, context) = deployment_job_channel(DeploymentId::from_u128(2));
        context
            .claim_status_sender()
            .send_replace(DeploymentClaimStatus::ReconciliationRequired);
        context
            .status_sender()
            .send_replace(DeploymentStatus::ReconciliationRequired(
                "outcome unknown".into(),
            ));
        let deployment_id = handle.id();
        let mut view = DeployStepView::new(handle);

        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char('r'),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(view.release_dialog.is_some());

        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(view.release_dialog.is_none());
        assert!(!view.release_pending);

        let _ = view.handle_key(KeyEvent::new(
            KeyCode::Char('r'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Message(AppMessage::Wizard(
                WizardMessage::ReleaseUnresolvedDeployment(deployment_id)
            ))
        );
        assert!(view.release_dialog.is_none());
        assert!(view.release_pending);
    }

    #[test]
    fn reconciled_unknown_history_does_not_offer_claim_release() {
        let (handle, context) = deployment_job_channel(DeploymentId::from_u128(3));
        context
            .claim_status_sender()
            .send_replace(DeploymentClaimStatus::Reconciled);
        context
            .status_sender()
            .send_replace(DeploymentStatus::ReconciliationRequired(
                "outcome unknown".into(),
            ));
        let mut view = DeployStepView::new(handle);

        let _ = view.handle_key(KeyEvent::new(
            KeyCode::Char('r'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(view.release_dialog.is_none());
        assert!(!view.release_pending);
    }
}
