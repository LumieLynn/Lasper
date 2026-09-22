use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
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
use crate::tui::core::{AppMessage, Component, ConfigurationMessage, EventResult, FocusTracker};
use crate::tui::theme;
use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::text_box::TextBox;
use crate::tui::widgets::lists::selectable_list::SelectableList;

macro_rules! active_components {
    ($self:ident) => {{
        let components: Vec<&mut dyn Component> = vec![
            &mut $self.sockets,
            &mut $self.guest_user,
            &mut $self.check,
            &mut $self.authorize,
            &mut $self.revoke,
            &mut $self.close,
        ];
        components
    }};
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum X11AccessDialogAction {
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

pub(crate) struct X11AccessDialog {
    machine: MachineName,
    sockets: SelectableList<HostX11Socket>,
    guest_user: TextBox,
    check: Button,
    authorize: Button,
    revoke: Button,
    close: Button,
    focus: FocusTracker,
    generation: u64,
    state: CheckState,
    authorization_confirmation: Option<PendingAuthorization>,
    revocation_confirmation: Option<PendingRevocation>,
    pending_check: Option<tokio::task::JoinHandle<()>>,
    pending_authorization: Option<tokio::task::JoinHandle<()>>,
    pending_revocation: Option<tokio::task::JoinHandle<()>>,
}

impl X11AccessDialog {
    pub(crate) fn new(
        machine: MachineName,
        sockets: Vec<HostX11Socket>,
        initially_selected: Option<&HostX11Socket>,
    ) -> Self {
        let mut sockets = SelectableList::new(" X11 displays ", sockets, socket_label);
        if let Some(selected) = initially_selected {
            if let Some(index) = sockets.items().iter().position(|socket| socket == selected) {
                sockets.select(index);
            }
        }
        let mut dialog = Self {
            machine,
            sockets,
            guest_user: TextBox::new(" Guest user ", String::new())
                .with_validator(validate_guest_user),
            check: Button::new("Check access", || {
                AppMessage::Configuration(ConfigurationMessage::CheckX11)
            }),
            authorize: Button::new("Authorize", || {
                AppMessage::Configuration(ConfigurationMessage::AuthorizeX11)
            })
            .with_enabled(false),
            revoke: Button::new("Revoke", || {
                AppMessage::Configuration(ConfigurationMessage::RevokeX11)
            })
            .with_enabled(false),
            close: Button::new("Close", || {
                AppMessage::Configuration(ConfigurationMessage::CloseX11Access)
            }),
            focus: FocusTracker::new(),
            generation: 0,
            state: CheckState::Untested,
            authorization_confirmation: None,
            revocation_confirmation: None,
            pending_check: None,
            pending_authorization: None,
            pending_revocation: None,
        };
        dialog.refresh_controls();
        dialog.update_focus();
        dialog
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.refresh_controls();
        let dialog = crate::tui::centered_rect(88, 92, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(format!(" X11 runtime access: {} ", self.machine))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let rows = Layout::vertical([
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);
        self.sockets.render(frame, rows[0]);
        self.guest_user.render(frame, rows[1]);
        self.render_status(frame, rows[2]);
        self.render_history(frame, rows[3]);

        let buttons = Layout::horizontal([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(rows[4]);
        self.check.render(frame, buttons[0]);
        self.authorize.render(frame, buttons[1]);
        self.revoke.render(frame, buttons[2]);
        self.close.render(frame, buttons[3]);
        frame.render_widget(
            Paragraph::new(" Tab/Shift+Tab focus, j/k select display, c check, a authorize, d revoke, Esc close ")
                .style(Style::default().fg(theme::theme().hint_fg)),
            rows[5],
        );

        if self.authorization_confirmation.is_some() {
            self.render_authorization_confirmation(frame, area);
        } else if self.revocation_confirmation.is_some() {
            self.render_revocation_confirmation(frame, area);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> X11AccessDialogAction {
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
            return X11AccessDialogAction::None;
        }
        match key.code {
            KeyCode::Esc => return X11AccessDialogAction::Close,
            KeyCode::Tab => {
                self.next_focus();
                return X11AccessDialogAction::None;
            }
            KeyCode::BackTab => {
                self.previous_focus();
                return X11AccessDialogAction::None;
            }
            KeyCode::Char('c') if !self.guest_user.is_focused() => return self.request_check(),
            KeyCode::Char('a') if !self.guest_user.is_focused() => {
                return self.request_authorization_confirmation()
            }
            KeyCode::Char('d') if !self.guest_user.is_focused() => {
                return self.request_revocation_confirmation()
            }
            _ => {}
        }

        let selected_before = self.sockets.selected_idx();
        let user_before = self.guest_user.value().to_owned();
        let result = {
            let mut components = active_components!(self);
            components[self.focus.active_idx].handle_key(key)
        };
        if self.sockets.selected_idx() != selected_before || self.guest_user.value() != user_before
        {
            self.invalidate_check();
        }
        match result {
            EventResult::FocusNext => self.next_focus(),
            EventResult::FocusPrev => self.previous_focus(),
            EventResult::Message(AppMessage::Configuration(message)) => {
                return self.handle_message(message)
            }
            _ => {}
        }
        X11AccessDialogAction::None
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
        self.update_focus();
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
        self.update_focus();
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
        self.update_focus();
    }

    fn update_focus(&mut self) {
        let mut components = active_components!(self);
        if self.focus.active_idx >= components.len() {
            self.focus.active_idx = components.len().saturating_sub(1);
        }
        if !components[self.focus.active_idx].is_focusable() {
            let start = self.focus.active_idx;
            loop {
                self.focus.active_idx = (self.focus.active_idx + 1) % components.len();
                if components[self.focus.active_idx].is_focusable()
                    || self.focus.active_idx == start
                {
                    break;
                }
            }
        }
        self.focus.update_focus(&mut components, true);
    }

    fn next_focus(&mut self) {
        let components = active_components!(self);
        self.focus.next(&components);
        drop(components);
        self.update_focus();
    }

    fn previous_focus(&mut self) {
        let components = active_components!(self);
        self.focus.prev(&components);
        drop(components);
        self.update_focus();
    }

    fn refresh_controls(&mut self) {
        self.check.set_enabled(self.selected_socket().is_some());
        self.authorize.set_enabled(self.can_authorize());
        self.revoke.set_enabled(self.can_revoke());
    }

    fn selected_socket(&self) -> Option<HostX11Socket> {
        self.sockets.selected_item().cloned()
    }

    fn target(&mut self) -> Result<ShellTarget, String> {
        self.guest_user.validate()?;
        let user = ValidatedGuestUserName::new(self.guest_user.value())
            .map_err(|error| error.to_string())?;
        Ok(ShellTarget::new(self.machine.clone(), user))
    }

    fn handle_message(&mut self, message: ConfigurationMessage) -> X11AccessDialogAction {
        match message {
            ConfigurationMessage::CheckX11 => self.request_check(),
            ConfigurationMessage::AuthorizeX11 => self.request_authorization_confirmation(),
            ConfigurationMessage::RevokeX11 => self.request_revocation_confirmation(),
            ConfigurationMessage::CloseX11Access => X11AccessDialogAction::Close,
        }
    }

    fn request_check(&mut self) -> X11AccessDialogAction {
        let Some(host_socket) = self.selected_socket() else {
            self.set_check_error("No local X11 display is available");
            return X11AccessDialogAction::None;
        };
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AccessDialogAction::None;
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
        X11AccessDialogAction::Check {
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

    fn request_authorization_confirmation(&mut self) -> X11AccessDialogAction {
        if !self.can_authorize() {
            return X11AccessDialogAction::None;
        }
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AccessDialogAction::None;
            }
        };
        let CheckState::Ready { check, .. } = &self.state else {
            return X11AccessDialogAction::None;
        };
        self.authorization_confirmation = Some(PendingAuthorization {
            target,
            host_socket: check.projection().host_socket().clone(),
            host_uid: check.projection().identity().host_uid(),
        });
        X11AccessDialogAction::None
    }

    fn request_revocation_confirmation(&mut self) -> X11AccessDialogAction {
        if !self.can_revoke() {
            return X11AccessDialogAction::None;
        }
        let target = match self.target() {
            Ok(target) => target,
            Err(error) => {
                self.set_check_error(error);
                return X11AccessDialogAction::None;
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
            return X11AccessDialogAction::None;
        };
        self.revocation_confirmation = Some(PendingRevocation {
            target,
            host_socket: check.projection().host_socket().clone(),
            record_id: record_id.clone(),
            host_uid: check.projection().identity().host_uid(),
        });
        X11AccessDialogAction::None
    }

    fn handle_authorization_confirmation(&mut self, key: KeyEvent) -> X11AccessDialogAction {
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
                X11AccessDialogAction::Authorize {
                    generation,
                    target: intent.target,
                    host_socket: intent.host_socket,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.authorization_confirmation = None;
                X11AccessDialogAction::None
            }
            _ => X11AccessDialogAction::None,
        }
    }

    fn handle_revocation_confirmation(&mut self, key: KeyEvent) -> X11AccessDialogAction {
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
                X11AccessDialogAction::Revoke {
                    generation,
                    target: intent.target,
                    host_socket: intent.host_socket,
                    record_id: intent.record_id,
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.revocation_confirmation = None;
                X11AccessDialogAction::None
            }
            _ => X11AccessDialogAction::None,
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

    fn render_status(&self, frame: &mut Frame, area: Rect) {
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
                &record_id[..record_id.len().min(12)]
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
                        &record_id[..record_id.len().min(12)]
                    ),
                    (_, Some(RevocationPresentation::AlreadyAbsent { record_id })) => format!(
                        " Last action finalized already-absent record {}.",
                        &record_id[..record_id.len().min(12)]
                    ),
                    (Some(AuthorizationPresentation::Added { record_id }), _) => format!(
                        " Last action created record {}.",
                        &record_id[..record_id.len().min(12)]
                    ),
                    (Some(AuthorizationPresentation::PreExisting), _) => {
                        " Last action reused the existing entry without claiming it.".to_owned()
                    }
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
                    grant_status(assessment, identity.host_uid()),
                    action,
                    acl_summary(check.acl()),
                )
            }
            CheckState::Failed {
                generation,
                message,
            } => format!("Check #{generation} failed: {message}"),
        };
        frame.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }).block(
                Block::default()
                    .title(" Current access ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            ),
            area,
        );
    }

    fn render_history(&self, frame: &mut Frame, area: Rect) {
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
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(theme::theme().text_secondary))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(" Operation history ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
                ),
            area,
        );
    }

    fn render_authorization_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(intent) = &self.authorization_confirmation else {
            return;
        };
        let dialog = centered_fixed(76, 13, area);
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new(format!(
                "Allow host UID {} to open new connections to X11 display :{}?\n\nThis ACL entry applies to every host process with that UID, not only {}@{}. It remains until explicitly revoked, removed externally, or the X server resets. Closing Lasper does not revoke it.\n\n[y] Authorize    [n/Esc] Cancel",
                intent.host_uid,
                intent.host_socket.display(),
                intent.target.user(),
                intent.target.machine(),
            ))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Authorize X11 access? ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme::theme().dialog_border_warn)),
            ),
            dialog,
        );
    }

    fn render_revocation_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(intent) = &self.revocation_confirmation else {
            return;
        };
        let dialog = centered_fixed(76, 12, area);
        frame.render_widget(Clear, dialog);
        frame.render_widget(
            Paragraph::new(format!(
                "Remove Lasper-managed localuser:#{} access from X11 display :{}?\n\nThis removes access for every host process with that UID. The record is retained as revoked for audit history.\n\n[y] Revoke    [n/Esc] Cancel",
                intent.host_uid,
                intent.host_socket.display(),
            ))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Revoke X11 access? ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme::theme().dialog_border_warn)),
            ),
            dialog,
        );
    }
}

impl Drop for X11AccessDialog {
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

fn centered_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
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

fn grant_status(assessment: &GrantAssessment, host_uid: u32) -> String {
    match &assessment.status {
        GrantStatus::AccessControlDisabled => "X server access control is disabled.".to_owned(),
        GrantStatus::Managed { record_id } => format!(
            "Exact localuser:#{host_uid} entry is managed by Lasper record {}.",
            &record_id[..record_id.len().min(12)]
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
        &latest.record_id[..latest.record_id.len().min(12)],
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
