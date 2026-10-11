use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::application::sessions::{ShellTarget, ValidatedGuestUserName};
use crate::application::x11::{
    X11AccessCheck, X11AccessError, X11Authorization, X11GrantAssessmentStatus,
    X11MappedUidAclStatus,
};
use crate::domain::machine::MachineName;
use crate::domain::x11::HostX11Socket;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::inputs::{
    button::ButtonControl,
    text_input::{Input, TextInput},
};
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::protocol::Component;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccessControl {
    Sockets,
    Guest,
    Status,
    History,
    Check,
    Authorize,
    Revoke,
    Close,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum X11AuthorizationDialogAction {
    None,
    Close,
    Check {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
    },
    Authorize {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
    },
    Revoke {
        generation: u64,
        target: ShellTarget,
        host_socket: HostX11Socket,
        record_id: String,
    },
}

enum CheckState {
    Untested,
    Loading {
        generation: u64,
        user: ValidatedGuestUserName,
    },
    Authorizing {
        generation: u64,
        user: ValidatedGuestUserName,
        host_uid: u32,
    },
    Revoking {
        generation: u64,
        user: ValidatedGuestUserName,
        record_id: String,
    },
    Ready {
        generation: u64,
        check: Box<X11AccessCheck>,
        assessment: GrantAssessment,
        authorization: Option<AuthorizationPresentation>,
        revocation: Option<RevocationPresentation>,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AuthorizationPresentation {
    AccessControlDisabled,
    PreExisting,
    ReusedManaged { record_id: String },
    Added { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RevocationPresentation {
    Revoked { record_id: String },
    AlreadyAbsent { record_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GrantStatus {
    AccessControlDisabled,
    Managed { record_id: String },
    PreExisting,
    Historical,
    Absent,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrantHistoryStatus {
    Managed,
    Historical,
    Absent,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GrantHistory {
    record_id: String,
    host_uid: u32,
    status: GrantHistoryStatus,
    detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GrantAssessment {
    status: GrantStatus,
    history: Vec<GrantHistory>,
    diagnostics: Vec<String>,
    records_complete: bool,
}

impl GrantAssessment {
    fn from_check(check: &X11AccessCheck) -> Self {
        let assessment = check.grant_assessment();
        let status = match assessment.status() {
            X11GrantAssessmentStatus::AccessControlDisabled => GrantStatus::AccessControlDisabled,
            X11GrantAssessmentStatus::Managed { record_id } => GrantStatus::Managed {
                record_id: record_id.clone(),
            },
            X11GrantAssessmentStatus::PreExisting => GrantStatus::PreExisting,
            X11GrantAssessmentStatus::Historical => GrantStatus::Historical,
            X11GrantAssessmentStatus::Absent => GrantStatus::Absent,
            X11GrantAssessmentStatus::OutcomeUnknown => GrantStatus::OutcomeUnknown,
        };
        Self {
            status,
            history: assessment
                .history()
                .iter()
                .map(|entry| GrantHistory {
                    record_id: entry.record_id().to_owned(),
                    host_uid: entry.host_uid(),
                    status: match entry.status() {
                        crate::application::x11::X11GrantHistoryStatus::Managed => {
                            GrantHistoryStatus::Managed
                        }
                        crate::application::x11::X11GrantHistoryStatus::Historical => {
                            GrantHistoryStatus::Historical
                        }
                        crate::application::x11::X11GrantHistoryStatus::Absent => {
                            GrantHistoryStatus::Absent
                        }
                        crate::application::x11::X11GrantHistoryStatus::OutcomeUnknown => {
                            GrantHistoryStatus::OutcomeUnknown
                        }
                    },
                    detail: entry.detail().to_owned(),
                })
                .collect(),
            diagnostics: assessment.diagnostics().to_vec(),
            records_complete: assessment.records_complete(),
        }
    }
}

#[derive(Clone, Debug)]
struct PendingAuthorization {
    target: ShellTarget,
    host_socket: HostX11Socket,
    host_uid: u32,
}

#[derive(Clone, Debug)]
struct PendingRevocation {
    target: ShellTarget,
    host_socket: HostX11Socket,
    record_id: String,
    host_uid: u32,
}

pub(crate) struct X11AuthorizationDialog {
    machine: MachineName,
    sockets: Vec<HostX11Socket>,
    selected: ListControlState,
    guest_user: Input,
    user_error: Option<String>,
    form: FormState<AccessControl>,
    generation: u64,
    state: CheckState,
    authorization_confirmation: Option<PendingAuthorization>,
    revocation_confirmation: Option<PendingRevocation>,
    pending_check: Option<tokio::task::JoinHandle<()>>,
    pending_authorization: Option<tokio::task::JoinHandle<()>>,
    pending_revocation: Option<tokio::task::JoinHandle<()>>,
}

impl X11AuthorizationDialog {
    pub(crate) fn new(
        machine: MachineName,
        sockets: Vec<HostX11Socket>,
        initially_selected: Option<&HostX11Socket>,
    ) -> Self {
        let selected = initially_selected
            .and_then(|selected| sockets.iter().position(|socket| socket == selected))
            .unwrap_or(0);
        let mut dialog = Self {
            machine,
            sockets,
            selected: ListControlState::new(selected),
            guest_user: Input::default(),
            user_error: None,
            form: FormState::new(AccessControl::Sockets),
            generation: 0,
            state: CheckState::Untested,
            authorization_confirmation: None,
            revocation_confirmation: None,
            pending_check: None,
            pending_authorization: None,
            pending_revocation: None,
        };
        dialog.form.set_focus(true);
        dialog.refresh_controls();
        dialog
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, AccessControl>) -> R) -> R {
        let available = self.selected_socket().is_some();
        let can_authorize = self.can_authorize();
        let can_revoke = self.can_revoke();
        let entries: Vec<_> = self
            .sockets
            .iter()
            .map(|socket| ListEntry::new(socket_label(socket)))
            .collect();
        let mut status = ContentControl::new(self.status_widget());
        let mut history = ContentControl::new(self.history_widget());
        let mut sockets = ListControl::new(&mut self.selected, " X11 displays ", &entries);
        let mut guest = TextInput::new(
            &mut self.guest_user,
            " Guest user ",
            self.user_error.as_deref(),
        );
        let mut check = ButtonControl::new("Check access");
        let mut authorize = ButtonControl::new("Authorize");
        let mut revoke = ButtonControl::new("Revoke");
        let mut close = ButtonControl::new("Close");
        visit(&mut Form::new(
            &mut self.form,
            [
                FormItem::field(AccessControl::Sockets, &mut sockets).height(6),
                FormItem::field(AccessControl::Guest, &mut guest),
                FormItem::field(AccessControl::Status, &mut status).fill(7),
                FormItem::field(AccessControl::History, &mut history).height(4),
                FormItem::row([
                    FormItem::field(AccessControl::Check, &mut check).with_enabled(available),
                    FormItem::field(AccessControl::Authorize, &mut authorize)
                        .with_enabled(can_authorize),
                    FormItem::field(AccessControl::Revoke, &mut revoke).with_enabled(can_revoke),
                    FormItem::field(AccessControl::Close, &mut close),
                ]),
            ],
        ))
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        if self.authorization_confirmation.is_some() || self.revocation_confirmation.is_some() {
            return;
        }
        if matches!(
            self.state,
            CheckState::Authorizing { .. } | CheckState::Revoking { .. }
        ) {
            footer.show_notice("Updating X11 authorization...");
            return;
        }
        self.form.register_footer_hints(footer);
        footer.register(
            FooterHint::new("Esc", "close")
                .primary()
                .on_key(KeyCode::Esc),
        );
        if !self.form.captures_text_input() {
            if self.selected_socket().is_some() {
                footer.register(FooterHint::new("c", "check").on_key(KeyCode::Char('c')));
            }
            if self.can_authorize() {
                footer.register(FooterHint::new("a", "authorize").on_key(KeyCode::Char('a')));
            }
            if self.can_revoke() {
                footer.register(FooterHint::new("d", "revoke").on_key(KeyCode::Char('d')));
            }
        }
    }

    pub(crate) fn render_layer(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        let active = foreground
            && self.authorization_confirmation.is_none()
            && self.revocation_confirmation.is_none()
            && !matches!(
                self.state,
                CheckState::Authorizing { .. } | CheckState::Revoking { .. }
            );
        self.form.set_focus(active);
        let dialog = crate::tui::foundation::geometry::centered_rect(88, 92, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(format!(" X11 authorization: {} ", self.machine))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        self.with_form(|form| form.render(frame, inner));

        if self.authorization_confirmation.is_some() {
            self.render_authorization_confirmation(frame, area);
        } else if self.revocation_confirmation.is_some() {
            self.render_revocation_confirmation(frame, area);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> X11AuthorizationDialogAction {
        if self.authorization_confirmation.is_some() {
            return self.handle_authorization_confirmation(key);
        }
        if self.revocation_confirmation.is_some() {
            return self.handle_revocation_confirmation(key);
        }
        if matches!(
            self.state,
            CheckState::Authorizing { .. } | CheckState::Revoking { .. }
        ) {
            return X11AuthorizationDialogAction::None;
        }
        match key.code {
            KeyCode::Esc => return X11AuthorizationDialogAction::Close,
            KeyCode::Char('c') if !self.form.captures_text_input() => return self.request_check(),
            KeyCode::Char('a') if !self.form.captures_text_input() => {
                return self.request_authorization_confirmation()
            }
            KeyCode::Char('d') if !self.form.captures_text_input() => {
                return self.request_revocation_confirmation()
            }
            _ => {}
        }

        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_control_input(outcome)
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> X11AuthorizationDialogAction {
        if self.authorization_confirmation.is_some()
            || self.revocation_confirmation.is_some()
            || matches!(
                self.state,
                CheckState::Authorizing { .. } | CheckState::Revoking { .. }
            )
        {
            return X11AuthorizationDialogAction::None;
        }
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_control_input(outcome)
    }

    fn finish_control_input(
        &mut self,
        outcome: FormOutcome<AccessControl>,
    ) -> X11AuthorizationDialogAction {
        match outcome {
            FormOutcome::Changed(AccessControl::Sockets | AccessControl::Guest) => {
                self.invalidate_check()
            }
            FormOutcome::Activated(AccessControl::Check) => return self.request_check(),
            FormOutcome::Activated(AccessControl::Authorize) => {
                return self.request_authorization_confirmation()
            }
            FormOutcome::Activated(AccessControl::Revoke) => {
                return self.request_revocation_confirmation()
            }
            FormOutcome::Activated(AccessControl::Close) => {
                return X11AuthorizationDialogAction::Close
            }
            _ => {}
        }
        X11AuthorizationDialogAction::None
    }

    pub(crate) fn track_check(&mut self, task: tokio::task::JoinHandle<()>) {
        if let Some(previous) = self.pending_check.replace(task) {
            previous.abort();
        }
    }

    pub(crate) fn track_authorization(&mut self, task: tokio::task::JoinHandle<()>) {
        self.pending_authorization = Some(task);
    }

    pub(crate) fn track_revocation(&mut self, task: tokio::task::JoinHandle<()>) {
        self.pending_revocation = Some(task);
    }

    pub(crate) fn finish_check(
        &mut self,
        generation: u64,
        result: Result<X11AccessCheck, X11AccessError>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending_check.take();
        self.state = match result {
            Ok(check) => CheckState::Ready {
                generation,
                assessment: GrantAssessment::from_check(&check),
                check: Box::new(check),
                authorization: None,
                revocation: None,
            },
            Err(error) => CheckState::Failed {
                generation,
                message: error.to_string(),
            },
        };
        self.refresh_controls();
    }

    pub(crate) fn finish_authorization(
        &mut self,
        generation: u64,
        result: Result<X11Authorization, X11AccessError>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending_authorization.take();
        self.state = match result {
            Ok(authorization) => {
                let presentation = match authorization.disposition() {
                    crate::application::x11::X11AuthorizationDisposition::AccessControlDisabled => {
                        AuthorizationPresentation::AccessControlDisabled
                    }
                    crate::application::x11::X11AuthorizationDisposition::PreExisting => {
                        AuthorizationPresentation::PreExisting
                    }
                    crate::application::x11::X11AuthorizationDisposition::ReusedManaged {
                        record_id,
                    } => AuthorizationPresentation::ReusedManaged {
                        record_id: record_id.clone(),
                    },
                    crate::application::x11::X11AuthorizationDisposition::Added { record_id } => {
                        AuthorizationPresentation::Added {
                            record_id: record_id.clone(),
                        }
                    }
                };
                CheckState::Ready {
                    generation,
                    assessment: GrantAssessment::from_check(authorization.check()),
                    check: Box::new(authorization.check().clone()),
                    authorization: Some(presentation),
                    revocation: None,
                }
            }
            Err(error) => CheckState::Failed {
                generation,
                message: error.to_string(),
            },
        };
        self.refresh_controls();
    }

    pub(crate) fn finish_revocation(
        &mut self,
        generation: u64,
        result: Result<crate::application::x11::X11Revocation, X11AccessError>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending_revocation.take();
        self.state = match result {
            Ok(revocation) => {
                let presentation = match revocation.disposition() {
                    crate::application::x11::X11RevocationDisposition::Revoked { record_id } => {
                        RevocationPresentation::Revoked {
                            record_id: record_id.clone(),
                        }
                    }
                    crate::application::x11::X11RevocationDisposition::AlreadyAbsent {
                        record_id,
                    } => RevocationPresentation::AlreadyAbsent {
                        record_id: record_id.clone(),
                    },
                };
                CheckState::Ready {
                    generation,
                    assessment: GrantAssessment::from_check(revocation.check()),
                    check: Box::new(revocation.check().clone()),
                    authorization: None,
                    revocation: Some(presentation),
                }
            }
            Err(error) => CheckState::Failed {
                generation,
                message: error.to_string(),
            },
        };
        self.refresh_controls();
    }

    fn refresh_controls(&mut self) {
        self.with_form(|_| ());
    }

    fn selected_socket(&self) -> Option<HostX11Socket> {
        self.selected
            .selected()
            .and_then(|index| self.sockets.get(index))
            .cloned()
    }

    fn target(&mut self) -> Result<ShellTarget, String> {
        let result = validate_guest_user(self.guest_user.value());
        self.user_error = result.as_ref().err().cloned();
        result?;
        let user = ValidatedGuestUserName::new(self.guest_user.value())
            .map_err(|error| error.to_string())?;
        Ok(ShellTarget::new(self.machine.clone(), user))
    }

    fn request_check(&mut self) -> X11AuthorizationDialogAction {
        let Some(host_socket) = self.selected_socket() else {
            self.set_check_error("No local X11 display is available");
            return X11AuthorizationDialogAction::None;
        };
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AuthorizationDialogAction::None;
            }
        };
        if let Some(previous) = self.pending_check.take() {
            previous.abort();
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("X11 check generation exhausted");
        let generation = self.generation;
        self.state = CheckState::Loading {
            generation,
            user: target.user().clone(),
        };
        X11AuthorizationDialogAction::Check {
            generation,
            target,
            host_socket,
        }
    }

    fn can_authorize(&self) -> bool {
        matches!(
            &self.state,
            CheckState::Ready { check, assessment, .. }
                if check.mapped_uid_status() == X11MappedUidAclStatus::ExactNumericEntryAbsent
                    && check.server_identity_trackable()
                    && assessment.records_complete
        ) && self.pending_authorization.is_none()
    }

    fn can_revoke(&self) -> bool {
        matches!(
            &self.state,
            CheckState::Ready { assessment, .. }
                if matches!(assessment.status, GrantStatus::Managed { .. })
        ) && self.pending_revocation.is_none()
    }

    fn request_authorization_confirmation(&mut self) -> X11AuthorizationDialogAction {
        if !self.can_authorize() {
            return X11AuthorizationDialogAction::None;
        }
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AuthorizationDialogAction::None;
            }
        };
        let CheckState::Ready { check, .. } = &self.state else {
            return X11AuthorizationDialogAction::None;
        };
        self.authorization_confirmation = Some(PendingAuthorization {
            target,
            host_socket: check.projection().host_socket().clone(),
            host_uid: check.projection().identity().host_uid(),
        });
        X11AuthorizationDialogAction::None
    }

    fn request_revocation_confirmation(&mut self) -> X11AuthorizationDialogAction {
        if !self.can_revoke() {
            return X11AuthorizationDialogAction::None;
        }
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AuthorizationDialogAction::None;
            }
        };
        let CheckState::Ready {
            check,
            assessment:
                GrantAssessment {
                    status: GrantStatus::Managed { record_id },
                    ..
                },
            ..
        } = &self.state
        else {
            return X11AuthorizationDialogAction::None;
        };
        self.revocation_confirmation = Some(PendingRevocation {
            target,
            host_socket: check.projection().host_socket().clone(),
            record_id: record_id.clone(),
            host_uid: check.projection().identity().host_uid(),
        });
        X11AuthorizationDialogAction::None
    }

    fn handle_authorization_confirmation(&mut self, key: KeyEvent) -> X11AuthorizationDialogAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let intent = self
                    .authorization_confirmation
                    .take()
                    .expect("authorization confirmation is visible");
                self.generation = self
                    .generation
                    .checked_add(1)
                    .expect("X11 check generation exhausted");
                let generation = self.generation;
                self.state = CheckState::Authorizing {
                    generation,
                    user: intent.target.user().clone(),
                    host_uid: intent.host_uid,
                };
                X11AuthorizationDialogAction::Authorize {
                    generation,
                    target: intent.target,
                    host_socket: intent.host_socket,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.authorization_confirmation = None;
                X11AuthorizationDialogAction::None
            }
            _ => X11AuthorizationDialogAction::None,
        }
    }

    fn handle_revocation_confirmation(&mut self, key: KeyEvent) -> X11AuthorizationDialogAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let intent = self
                    .revocation_confirmation
                    .take()
                    .expect("revocation confirmation is visible");
                self.generation = self
                    .generation
                    .checked_add(1)
                    .expect("X11 check generation exhausted");
                let generation = self.generation;
                self.state = CheckState::Revoking {
                    generation,
                    user: intent.target.user().clone(),
                    record_id: intent.record_id.clone(),
                };
                X11AuthorizationDialogAction::Revoke {
                    generation,
                    target: intent.target,
                    host_socket: intent.host_socket,
                    record_id: intent.record_id,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.revocation_confirmation = None;
                X11AuthorizationDialogAction::None
            }
            _ => X11AuthorizationDialogAction::None,
        }
    }

    fn invalidate_check(&mut self) {
        if let Some(task) = self.pending_check.take() {
            task.abort();
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("X11 check generation exhausted");
        self.state = CheckState::Untested;
        self.refresh_controls();
    }

    fn set_check_error(&mut self, message: impl Into<String>) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("X11 check generation exhausted");
        self.state = CheckState::Failed {
            generation: self.generation,
            message: message.into(),
        };
        self.refresh_controls();
    }

    fn status_widget(&self) -> Paragraph<'static> {
        let status = match &self.state {
            CheckState::Untested => {
                "Not queried. Select a display and guest user, then run Check access.".to_owned()
            }
            CheckState::Loading { generation, user } => {
                format!("Check #{generation}: validating projection and X server ACL for {user}...")
            }
            CheckState::Authorizing {
                generation,
                user,
                host_uid,
            } => format!(
                "Authorization #{generation}: adding exact localuser:#{host_uid} access for {user}..."
            ),
            CheckState::Revoking {
                generation,
                user,
                record_id,
            } => format!(
                "Revocation #{generation}: removing Lasper record {} for {user}...",
                short_record_id(record_id)
            ),
            CheckState::Ready {
                generation,
                check,
                assessment,
                authorization,
                revocation,
            } => {
                let context = check.projection();
                let identity = context.identity();
                let action = match (authorization, revocation) {
                    (_, Some(RevocationPresentation::Revoked { record_id })) => format!(
                        " Last action revoked record {}.",
                        short_record_id(record_id)
                    ),
                    (_, Some(RevocationPresentation::AlreadyAbsent { record_id })) => format!(
                        " Last action finalized already-absent record {}.",
                        short_record_id(record_id)
                    ),
                    (Some(AuthorizationPresentation::Added { record_id }), _) => format!(
                        " Last action created record {}.",
                        short_record_id(record_id)
                    ),
                    (Some(AuthorizationPresentation::PreExisting), _) => {
                        " Last action reused the existing entry without claiming it.".to_owned()
                    }
                    (Some(AuthorizationPresentation::ReusedManaged { record_id }), _) => format!(
                        " Last action registered this machine's claim on grant {}.",
                        short_record_id(record_id)
                    ),
                    (Some(AuthorizationPresentation::AccessControlDisabled), _) => {
                        " No entry was added.".to_owned()
                    }
                    (None, None) => String::new(),
                };
                format!(
                    "Check #{generation}: guest uid {} maps to host uid {}.\n{} -> {} -> {}\n{}\n{}{}\n{}",
                    identity.guest().uid(),
                    identity.host_uid(),
                    context.host_socket().source().display(),
                    context.guest_mount().display(),
                    context.guest_client_path().display(),
                    pathname_access_summary(context.filesystem_access()),
                    grant_status(
                        assessment,
                        identity.host_uid(),
                        check.server_identity_trackable(),
                    ),
                    action,
                    acl_summary(check.acl()),
                )
            }
            CheckState::Failed {
                generation,
                message,
            } => format!("Check #{generation} failed: {message}"),
        };
        Paragraph::new(status).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Current access ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        )
    }

    fn history_widget(&self) -> Paragraph<'static> {
        let text = match &self.state {
            CheckState::Ready { assessment, .. } => history_text(assessment),
            CheckState::Loading { .. }
            | CheckState::Authorizing { .. }
            | CheckState::Revoking { .. } => "Loading grant records...".to_owned(),
            CheckState::Failed { .. } => {
                "Current records were not assessed because the check failed.".to_owned()
            }
            CheckState::Untested => "Not queried.".to_owned(),
        };
        Paragraph::new(text)
            .style(Style::default().fg(theme::theme().text_secondary))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Operation history ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            )
    }

    fn render_authorization_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(intent) = &self.authorization_confirmation else {
            return;
        };
        ConfirmationDialog::new(
            "Authorize X11 access?",
            format!("Allow host UID {} to open new connections to X11 display :{}?\n\nThis ACL entry applies to every host process with that UID, not only {}@{}. It remains until explicitly revoked, removed externally, or the X server resets. Closing Lasper does not revoke it.",
                intent.host_uid, intent.host_socket.display(), intent.target.user(), intent.target.machine()),
        )
        .with_width(76)
        .with_hints(&[("y", "authorize"), ("n/Esc", "cancel")])
        .render(frame, area);
    }

    fn render_revocation_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(intent) = &self.revocation_confirmation else {
            return;
        };
        ConfirmationDialog::new(
            "Revoke X11 access?",
            format!("Remove Lasper-managed localuser:#{} access from X11 display :{}?\n\nThis removes access for every host process with that UID. The record is retained as revoked for audit history.",
                intent.host_uid, intent.host_socket.display()),
        )
        .with_width(76)
        .with_hints(&[("y", "revoke"), ("n/Esc", "cancel")])
        .render(frame, area);
    }
}

impl Drop for X11AuthorizationDialog {
    fn drop(&mut self) {
        if let Some(task) = self.pending_check.take() {
            task.abort();
        }
        // Authorization and revocation mutate the X server and the managed
        // record store. Detaching lets each operation reach a recorded outcome
        // even if the view closes before its result can be presented.
        self.pending_authorization.take();
        self.pending_revocation.take();
    }
}

fn socket_label(socket: &HostX11Socket) -> String {
    format!(
        ":{}  {}  uid {}  mode {:04o}",
        socket.display(),
        socket
            .source()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("endpoint"),
        socket.owner_uid(),
        socket.mode(),
    )
}

fn validate_guest_user(value: &str) -> Result<(), String> {
    ValidatedGuestUserName::new(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn pathname_access_summary(
    access: crate::application::sessions::X11FilesystemAccess,
) -> &'static str {
    if access.fully_writable() {
        return "Pathname transport: writable at both guest paths.";
    }
    match (access.mount_writable(), access.client_writable()) {
        (false, false) => "Pathname: denied at both guest paths; abstract route not tested.",
        (false, true) => "Pathname: denied at guest mount; abstract route not tested.",
        (true, false) => "Pathname: denied at client path; abstract route not tested.",
        (true, true) => unreachable!("handled above"),
    }
}

fn grant_status(
    assessment: &GrantAssessment,
    host_uid: u32,
    server_identity_trackable: bool,
) -> String {
    match &assessment.status {
        GrantStatus::AccessControlDisabled => "X server access control is disabled.".to_owned(),
        _ if !server_identity_trackable => {
            "X server peer is external to this PID namespace; Lasper will not manage ACL lifecycle for this display.".to_owned()
        }
        GrantStatus::Managed { record_id } => format!(
            "Exact localuser:#{host_uid} entry is managed by Lasper record {}.",
            short_record_id(record_id)
        ),
        GrantStatus::PreExisting => {
            format!("Exact localuser:#{host_uid} entry is present but is external/unmanaged.")
        }
        GrantStatus::Historical => format!(
            "Exact localuser:#{host_uid} entry has historical Lasper records, but current ownership is unconfirmed."
        ),
        GrantStatus::Absent => format!("No exact localuser:#{host_uid} entry was observed."),
        GrantStatus::OutcomeUnknown => format!(
            "Exact localuser:#{host_uid} ownership is unknown; managed actions are disabled."
        ),
    }
}

fn history_text(assessment: &GrantAssessment) -> String {
    let Some(latest) = assessment.history.first() else {
        return assessment
            .diagnostics
            .first()
            .map(|diagnostic| format!("No usable managed record. {diagnostic}"))
            .unwrap_or_else(|| {
                "No Lasper-created grant records match this machine, user, and display.".to_owned()
            });
    };
    let status = match latest.status {
        GrantHistoryStatus::Managed => "Managed",
        GrantHistoryStatus::Historical => "Historical",
        GrantHistoryStatus::Absent => "Absent",
        GrantHistoryStatus::OutcomeUnknown => "Outcome unknown",
    };
    let mut lines = vec![format!(
        "{status}: {} - host uid {}",
        short_record_id(&latest.record_id),
        latest.host_uid
    )];
    lines.push(latest.detail.clone());
    let remaining = assessment.history.len().saturating_sub(1);
    if remaining > 0 {
        lines.push(format!("{remaining} older matching record(s)"));
    } else if let Some(diagnostic) = assessment.diagnostics.first() {
        lines.push(format!("Record warning: {diagnostic}"));
    }
    lines.join("\n")
}

fn short_record_id(record_id: &str) -> &str {
    let end = record_id
        .char_indices()
        .nth(12)
        .map_or(record_id.len(), |(index, _)| index);
    &record_id[..end]
}

fn acl_summary(snapshot: &crate::application::x11::X11AclSnapshot) -> String {
    let local_users = snapshot
        .entries()
        .iter()
        .filter_map(|entry| entry.server_interpreted())
        .filter_map(|(kind, value)| (kind == "localuser").then_some(value))
        .collect::<Vec<_>>();
    if local_users.is_empty() {
        return format!(
            "ACL localuser entries: none ({} total).",
            snapshot.entries().len()
        );
    }
    let shown = local_users.iter().take(3).cloned().collect::<Vec<_>>();
    let more = local_users.len().saturating_sub(shown.len());
    format!(
        "ACL localuser entries: {}{} ({} total ACL entries).",
        shown.join(", "),
        if more == 0 {
            String::new()
        } else {
            format!(", +{more} more")
        },
        snapshot.entries().len(),
    )
}
