mod accounts;
mod logs;

use crate::application::provisioning::{
    DeploymentClaimStatus, DeploymentEvent, DeploymentJobHandle, DeploymentProgress,
    DeploymentStatus,
};
use crate::tui::features::wizard::draft::WizardDraft;
use crate::tui::features::wizard::steps::field_event;
use crate::tui::features::wizard::steps::StepComponent;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::components::form::{Form, FormItem, FormState};
use crate::tui::foundation::components::text_block::{TextBlockControl, TextBlockState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::control::Control;
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::display::text::PlainText;
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use logs::DeploymentLogs;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Gauge, Paragraph},
    Frame,
};

const MAX_DEPLOYMENT_EVENTS_PER_UPDATE: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeployControl {
    Status,
    Logs,
    Accounts,
}

pub struct DeployStepView {
    job: DeploymentJobHandle,
    status: String,
    finished: bool,
    claim_status: DeploymentClaimStatus,
    cancellation_requested: bool,
    focused: bool,
    log_view: TextBlockState,
    form: FormState<DeployControl>,
    logs: DeploymentLogs,
    progress: Option<DeploymentProgress>,
    cancel_dialog: Option<ConfirmationDialog>,
    release_dialog: Option<ConfirmationDialog>,
    release_pending: bool,
    account_content: Option<PlainText>,
    accounts: TextBlockState,
    result_presented: bool,
}

impl DeployStepView {
    pub fn new(job: DeploymentJobHandle) -> Self {
        let mut view = Self {
            job,
            status: "Deploying...".into(),
            finished: false,
            claim_status: DeploymentClaimStatus::Held,
            cancellation_requested: false,
            focused: true,
            log_view: TextBlockState::default(),
            form: FormState::new(DeployControl::Logs),
            logs: DeploymentLogs::default(),
            progress: None,
            cancel_dialog: None,
            release_dialog: None,
            release_pending: false,
            account_content: None,
            accounts: TextBlockState::default(),
            result_presented: false,
        };
        view.update_status();
        view.with_form(|_| ());
        view
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, DeployControl>) -> R) -> R {
        self.form.set_focus(
            self.focused && self.cancel_dialog.is_none() && self.release_dialog.is_none(),
        );
        let mut logs =
            TextBlockControl::new(&mut self.log_view, " Deployment logs ", self.logs.text())
                .follow_tail();
        let mut status = ContentControl::new(
            Paragraph::new(self.status.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(" Status "),
            ),
        );
        let mut progress = self
            .progress
            .as_ref()
            .filter(|_| !self.finished)
            .map(|progress| {
                ContentControl::new(
                    Gauge::default()
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .title(format!(" {} ", progress.label)),
                        )
                        .gauge_style(
                            Style::default().fg(crate::tui::foundation::theme::theme().accent),
                        )
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
        let mut accounts = self.account_content.as_mut().map(|content| {
            TextBlockControl::new(
                &mut self.accounts,
                " Local accounts (/etc/passwd snapshot) ",
                content,
            )
        });
        let mut fields = vec![
            FormItem::field(DeployControl::Status, status),
            FormItem::field(DeployControl::Logs, &mut logs).fill(4),
        ];
        if let Some(accounts) = &mut accounts {
            fields.push(FormItem::field(DeployControl::Accounts, accounts).fill(4));
        }
        visit(&mut Form::new(&mut self.form, fields))
    }

    fn update_logs(&mut self) {
        // A busy provider must leave time to render and handle input.
        for _ in 0..MAX_DEPLOYMENT_EVENTS_PER_UPDATE {
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

    fn update_status(&mut self) {
        let job_status = self.job.status();
        self.finished = job_status.is_finished();
        self.claim_status = self.job.claim_status();
        self.cancellation_requested = self.job.cancellation_requested();
        if self.finished {
            self.cancel_dialog = None;
        }
        if self.claim_status != DeploymentClaimStatus::ReconciliationRequired {
            self.release_dialog = None;
            self.release_pending = false;
        }

        let status = if *job_status == DeploymentStatus::RollingBack {
            "Rolling back deployment changes...".to_string()
        } else if !self.finished && self.cancellation_requested {
            "Cancellation requested; waiting for a safe rollback point...".to_string()
        } else {
            match &*job_status {
                DeploymentStatus::Running => "Deploying... please wait.".to_string(),
                DeploymentStatus::RollingBack => unreachable!(),
                DeploymentStatus::Succeeded(_) => "SUCCESS: Deployment completed.".to_string(),
                DeploymentStatus::Cancelled(message) => format!("CANCELLED: {message}"),
                DeploymentStatus::Failed(message) => format!("FAILED: {message}"),
                DeploymentStatus::ReconciliationRequired(message) => match self.claim_status {
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
        if !self.result_presented {
            if let DeploymentStatus::Succeeded(result) = &*job_status {
                self.account_content = result
                    .local_accounts
                    .as_ref()
                    .map(|accounts| PlainText::new(accounts::content(accounts)));
                self.result_presented = true;
            }
        }
        drop(job_status);
        self.status = status;
    }
}

impl Component for DeployStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if self.release_dialog.is_some() || self.cancel_dialog.is_some() {
            return;
        }
        if self.finished && self.release_pending {
            footer.show_notice("Discarding unresolved deployment state...");
            return;
        }
        self.form.register_footer_hints(footer);
        if self.finished {
            if self.claim_status == DeploymentClaimStatus::ReconciliationRequired {
                footer.register(
                    FooterHint::new("r", "discard unresolved state").on_key(KeyCode::Char('r')),
                );
            }
            footer.register(
                FooterHint::new("Enter/q/Esc", "close")
                    .primary()
                    .on_key(KeyCode::Esc),
            );
        } else if self.cancellation_requested {
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
        self.with_form(|form| form.render(f, area.inner(Margin::new(1, 1))));

        if let Some(dialog) = &mut self.cancel_dialog {
            dialog.render(f, area);
        }
        if let Some(dialog) = &mut self.release_dialog {
            dialog.render(f, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.finished {
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
                    if self.claim_status == DeploymentClaimStatus::ReconciliationRequired
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
                    self.update_status();
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
            KeyCode::Char('q') | KeyCode::Esc if !self.cancellation_requested => {
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

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.form
            .set_focus(focused && self.cancel_dialog.is_none() && self.release_dialog.is_none());
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for DeployStepView {
    fn poll_updates(&mut self) {
        self.update_logs();
        self.update_status();
    }

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
mod tests;
