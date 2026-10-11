use crate::application::provisioning::{
    DeploymentClaimStatus, DeploymentEvent, DeploymentJobHandle, DeploymentProgress,
    DeploymentStatus,
};
use crate::tui::features::wizard::draft::WizardDraft;
use crate::tui::features::wizard::steps::field_event;
use crate::tui::features::wizard::steps::StepComponent;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::text::soft_wrap_text;
use crate::tui::foundation::widgets::control::Control;
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeployControl {
    Status,
    Logs,
}

pub struct DeployStepView {
    job: DeploymentJobHandle,
    status: String,
    log_items: Vec<String>,
    selected: ListControlState,
    form: FormState<DeployControl>,
    logs: DeploymentLogs,
    progress: Option<DeploymentProgress>,
    cancel_dialog: Option<ConfirmationDialog>,
    release_dialog: Option<ConfirmationDialog>,
    release_pending: bool,
}

impl DeployStepView {
    pub fn new(job: DeploymentJobHandle) -> Self {
        let mut view = Self {
            job,
            status: "Deploying...".into(),
            log_items: Vec::new(),
            selected: ListControlState::default(),
            form: FormState::new(DeployControl::Logs),
            logs: DeploymentLogs::new(MAX_DEPLOYMENT_LOG_LINES),
            progress: None,
            cancel_dialog: None,
            release_dialog: None,
            release_pending: false,
        };
        view.form.set_focus(true);
        view.with_form(|_| ());
        view
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, DeployControl>) -> R) -> R {
        let done = self.job.status().is_finished();
        let entries: Vec<_> = self
            .log_items
            .iter()
            .map(|line| ListEntry::new(line.as_str()))
            .collect();
        let mut logs = ListControl::new(&mut self.selected, " Deployment logs ", &entries);
        let mut status = ContentControl::new(
            Paragraph::new(self.status.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(" Status "),
            ),
        );
        let mut progress = self.progress.as_ref().filter(|_| !done).map(|progress| {
            ContentControl::new(
                Gauge::default()
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .title(format!(" {} ", progress.label)),
                    )
                    .gauge_style(Style::default().fg(crate::tui::foundation::theme::theme().accent))
                    .ratio(f64::from(progress.permille()) / 1000.0)
                    .label(format!(
                        "{}.{:01}%",
                        progress.permille() / 10,
                        progress.permille() % 10
                    )),
            )
        });
        let status: &mut dyn Control = if let Some(progress) = &mut progress {
            progress
        } else {
            &mut status
        };
        visit(&mut Form::new(
            &mut self.form,
            [
                FormItem::field(DeployControl::Status, status),
                FormItem::field(DeployControl::Logs, &mut logs).fill(4),
            ],
        ))
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
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        let done = self.job.status().is_finished();
        if (done && self.release_dialog.is_some()) || (!done && self.cancel_dialog.is_some()) {
            return;
        }
        if done && self.release_pending {
            footer.show_notice("Discarding unresolved deployment state...");
            return;
        }
        self.form.register_footer_hints(footer);
        if done {
            if self.job.claim_status() == DeploymentClaimStatus::ReconciliationRequired {
                footer.register(
                    FooterHint::new("r", "discard unresolved state").on_key(KeyCode::Char('r')),
                );
            }
            footer.register(
                FooterHint::new("Enter/q/Esc", "close")
                    .primary()
                    .on_key(KeyCode::Esc),
            );
        } else if self.job.cancellation_requested() {
            footer.show_notice("Cancellation in progress");
        } else {
            footer.register(
                FooterHint::new("q/Esc", "cancel deployment")
                    .primary()
                    .on_key(KeyCode::Esc),
            );
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let area = area.inner(Margin::new(1, 1));

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
        self.status = status;

        self.update_logs();
        let log_list_was_empty = self.log_items.is_empty();
        let was_following_tail = self
            .selected
            .selected()
            .is_some_and(|selected| selected + 1 == self.log_items.len());
        let log_width = usize::from(area.width.saturating_sub(5).max(1));
        if let Some(wrapped) = self.logs.wrapped_if_changed(log_width) {
            self.log_items = wrapped;
            if log_list_was_empty || was_following_tail {
                self.selected.select(self.log_items.len().saturating_sub(1));
            }
        }

        self.form
            .set_focus(self.cancel_dialog.is_none() && self.release_dialog.is_none());
        self.with_form(|form| form.render(f, area));

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
                        "Discard Unresolved Deployment State?",
                        "Release Lasper's coordination claim and discard the durable recovery manifest? The deployment outcome remains unknown; inspect any host resources manually.",
                    ));
                    EventResult::Consumed
                }
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => {
                    EventResult::Message(AppMessage::Wizard(WizardMessage::Close))
                }
                _ => field_event(self.with_form(|form| form.handle_key(key))),
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
            _ => field_event(self.with_form(|form| form.handle_key(key))),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if self.release_dialog.is_some() || self.cancel_dialog.is_some() {
            return EventResult::Consumed;
        }
        field_event(self.with_form(|form| form.handle_mouse(mouse)))
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

    fn handle_message(&mut self, msg: &AppMessage) -> crate::tui::features::wizard::StepAction {
        let AppMessage::Wizard(WizardMessage::UnresolvedDeploymentReleaseFinished {
            deployment_id,
            error,
        }) = msg
        else {
            return crate::tui::features::wizard::StepAction::None;
        };
        if *deployment_id != self.job.id() {
            return crate::tui::features::wizard::StepAction::None;
        }
        self.release_pending = false;
        match error {
            Some(error) => {
                self.logs.push(format!("CLAIM RELEASE FAILED: {error}"));
                crate::tui::features::wizard::StepAction::Status(
                    format!("Could not release unresolved deployment claim: {error}"),
                    crate::tui::foundation::status::StatusLevel::Error,
                )
            }
            None => crate::tui::features::wizard::StepAction::Status(
                "Released the unresolved coordination claim; the historical deployment outcome remains unknown."
                    .into(),
                crate::tui::foundation::status::StatusLevel::Warn,
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
