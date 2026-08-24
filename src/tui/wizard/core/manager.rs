use crate::application::provisioning::{
    DeploymentError, DeploymentJobHandle, DeploymentPreflight, DeploymentRequest,
};
use crate::nspawn::{ContainerEntry, ImageEntry};
use crate::tui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::tui::wizard::core::draft::{SourceKind, WizardDraft};
use crate::tui::wizard::steps::{self, StepComponent};
use crate::tui::wizard::{StepAction, WizardStep};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

struct TarRiskConfirmationDialog {
    dialog: crate::tui::widgets::dialogs::confirmation::ConfirmationDialog,
}

impl TarRiskConfirmationDialog {
    fn new(risk: String) -> Self {
        Self {
            dialog: crate::tui::widgets::dialogs::confirmation::ConfirmationDialog::new(
                "Continue Remote Tar Import?",
                format!(
                    "{risk}\n\nThis remote archive will be extracted as root and may write outside the target through archive-created links. Continue only if you trust the archive source."
                ),
            ),
        }
    }
}

impl Component for TarRiskConfirmationDialog {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.dialog.render(frame, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::AcceptUnsafeRemoteTar))
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DeclineUnsafeRemoteTar))
            }
            _ => self.dialog.handle_key(key),
        }
    }
}

pub struct Wizard {
    id: crate::tui::wizard::WizardInstanceId,
    pub step: WizardStep,
    pub draft: WizardDraft,

    /// The active view for the current step.
    /// Recreated on step transitions to ensure fresh data from the draft.
    pub active_view: Option<Box<dyn StepComponent>>,

    permission_level: crate::composition::PermissionLevel,
    preparation: std::sync::Arc<crate::application::provisioning::ProvisioningPreparationService>,
    pending_request: Option<DeploymentRequest>,
    pub loading: bool,
}

impl Wizard {
    pub fn new(
        id: crate::tui::wizard::WizardInstanceId,
        entries: Vec<ContainerEntry>,
        images: Vec<ImageEntry>,
        permission_level: crate::composition::PermissionLevel,
        config: std::sync::Arc<crate::config::AppConfig>,
        host: crate::application::provisioning::ProvisioningHostSnapshot,
        preparation: std::sync::Arc<
            crate::application::provisioning::ProvisioningPreparationService,
        >,
    ) -> Self {
        let draft = WizardDraft::new(entries, images, config, host);

        Self {
            id,
            step: WizardStep::Source,
            draft,
            active_view: None,
            permission_level,
            preparation,
            pending_request: None,
            loading: false,
        }
    }

    pub fn id(&self) -> crate::tui::wizard::WizardInstanceId {
        self.id
    }

    /// Build the current step view on demand.
    fn sync_view(&mut self) {
        if self.active_view.is_none() && self.step != WizardStep::Deploy {
            self.active_view = Some(steps::build_view(
                self.step,
                &self.draft,
                self.preparation.clone(),
            ));
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        self.sync_view();

        let area = crate::tui::centered_rect(80, 80, area);
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(format!(
                " {} - Step {}/{} ",
                self.step.title(),
                self.step_index() + 1,
                self.total_steps()
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                ratatui::style::Style::default().fg(crate::tui::theme::theme().wizard_border),
            );

        let inner = block.inner(area);
        f.render_widget(block, area);

        if self.loading {
            let loading_area = crate::tui::centered_rect(30, 10, inner);
            let spinner = ratatui::widgets::Paragraph::new("\n  Processing... Please wait  ")
                .alignment(ratatui::layout::Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" Working "),
                );
            f.render_widget(spinner, loading_area);
            return;
        }

        // Layout: Remove the extra Length(1) phantom row
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(inner);

        if let Some(view) = &mut self.active_view {
            // Render against the current draft so previews remain reactive.
            view.render_step(f, chunks[0], &self.draft);
        }
    }

    pub fn active_flow(&self) -> Vec<WizardStep> {
        let is_copy = self.draft.source.kind == SourceKind::Copy;
        let is_oci = self.draft.source.kind == SourceKind::Oci;

        if is_copy {
            vec![
                WizardStep::Source,
                WizardStep::CopySelect,
                WizardStep::Basic,
                WizardStep::Review,
                WizardStep::Deploy,
            ]
        } else if is_oci {
            vec![
                WizardStep::Source,
                WizardStep::Basic,
                WizardStep::Review,
                WizardStep::Deploy,
            ]
        } else {
            let mut flow = vec![
                WizardStep::Source,
                WizardStep::Basic,
                WizardStep::User,
                WizardStep::Network,
                WizardStep::HostIntegration,
                WizardStep::BindMounts,
                WizardStep::Review,
                WizardStep::Deploy,
            ];

            if !self.draft.source.is_storage_managed_externally() {
                flow.insert(2, WizardStep::Storage);
            }
            flow
        }
    }

    fn total_steps(&self) -> usize {
        self.active_flow().len()
    }

    fn step_index(&self) -> usize {
        self.active_flow()
            .iter()
            .position(|&s| s == self.step)
            .unwrap_or(0)
    }

    fn resolve_next_step(&self, current: WizardStep) -> Option<WizardStep> {
        let flow = self.active_flow();
        let idx = flow.iter().position(|&s| s == current)?;
        flow.get(idx + 1).copied()
    }

    fn resolve_prev_step(&self, current: WizardStep) -> Option<WizardStep> {
        let flow = self.active_flow();
        let idx = flow.iter().position(|&s| s == current)?;
        if idx > 0 {
            Some(flow[idx - 1])
        } else {
            None
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> StepAction {
        if self.loading {
            return StepAction::None;
        }

        // Input can arrive before the first frame after a step transition.
        // Build the view here as well so Deploy always applies its confirmation
        // semantics instead of falling through to wizard-global shortcuts.
        self.sync_view();

        let res = if let Some(view) = &mut self.active_view {
            let result = view.handle_key(key);
            view.commit_to_draft(&mut self.draft);
            result
        } else {
            EventResult::Ignored
        };

        match res {
            EventResult::Message(msg) => self.process_message(msg),
            EventResult::Consumed => StepAction::None,
            EventResult::FocusNext | EventResult::FocusPrev => StepAction::None,
            EventResult::Ignored => match key.code {
                KeyCode::Esc => self.handle_action(StepAction::Prev),
                KeyCode::Char('q') => StepAction::Close,
                KeyCode::Enter => self.handle_action(StepAction::Next),
                _ => StepAction::None,
            },
        }
    }

    /// Route mouse input only to the active wizard step.  The parent App
    /// treats the wizard as modal, so an ignored event is still prevented
    /// from reaching the main panels.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if self.loading {
            return EventResult::Consumed;
        }
        if let Some(view) = &mut self.active_view {
            let result = view.handle_mouse(mouse);
            view.commit_to_draft(&mut self.draft);
            result
        } else {
            EventResult::Ignored
        }
    }

    pub fn process_message(&mut self, msg: AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(ref wiz_msg) => match wiz_msg {
                WizardMessage::Close => StepAction::Close,
                WizardMessage::Submit => self.submit_config(),
                WizardMessage::AcceptUnsafeRemoteTar => {
                    if !self.draft.source.accept_unsafe_remote_tar() {
                        return StepAction::Status(
                            "The remote tar source changed before confirmation; review it and try again."
                                .into(),
                            crate::tui::StatusLevel::Error,
                        );
                    }
                    self.submit_config()
                }
                WizardMessage::DeclineUnsafeRemoteTar => StepAction::CloseDialog,
                WizardMessage::ReleaseUnresolvedDeployment(deployment_id) => {
                    StepAction::ReleaseUnresolvedDeployment(*deployment_id)
                }
                WizardMessage::OpenUserDialog => {
                    let wayland_owner = self
                        .draft
                        .user
                        .users
                        .iter()
                        .find(|user| user.wayland.is_some())
                        .map(|user| user.username.clone());
                    let mut editor = crate::tui::widgets::dialogs::user_editor::UserEditor::new(
                        self.draft.passthrough.wayland_sockets.clone(),
                        wayland_owner.as_deref(),
                        |u| AppMessage::Wizard(WizardMessage::UserAdded(u)),
                    );
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenUserEditDialog(idx, ref user) => {
                    let idx = *idx;
                    let wayland_owner = self
                        .draft
                        .user
                        .users
                        .iter()
                        .enumerate()
                        .find(|(user_idx, user)| *user_idx != idx && user.wayland.is_some())
                        .map(|(_, user)| user.username.clone());
                    let mut editor = crate::tui::widgets::dialogs::user_editor::UserEditor::new(
                        self.draft.passthrough.wayland_sockets.clone(),
                        wayland_owner.as_deref(),
                        move |u| AppMessage::Wizard(WizardMessage::UserUpdated(idx, u)),
                    )
                    .with_user(user);
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenPortDialog => {
                    let mut editor =
                        crate::tui::widgets::dialogs::port_mapping::PortMappingBox::new(|p| {
                            AppMessage::Wizard(WizardMessage::PortForwardAdded(p))
                        });
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenPortEditDialog(idx, ref pf) => {
                    let idx = *idx;
                    let mut editor =
                        crate::tui::widgets::dialogs::port_mapping::PortMappingBox::new(move |p| {
                            AppMessage::Wizard(WizardMessage::PortForwardUpdated(idx, p))
                        })
                        .with_port(pf);
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenBindDialog => {
                    let mut editor =
                        crate::tui::widgets::dialogs::bind_mount::BindMountBox::new(|b| {
                            AppMessage::Wizard(WizardMessage::BindMountAdded(b))
                        });
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenBindEditDialog(idx, ref bm) => {
                    let idx = *idx;
                    let mut editor =
                        crate::tui::widgets::dialogs::bind_mount::BindMountBox::new(move |b| {
                            AppMessage::Wizard(WizardMessage::BindMountUpdated(idx, b))
                        })
                        .with_mount(bm);
                    editor.set_focus(true);
                    StepAction::OpenDialog(Box::new(editor))
                }
                WizardMessage::OpenNvidiaConfigDialog => {
                    let gpu_devices = self.draft.passthrough.nvidia_available_devices.clone();
                    let active_cats = self.draft.passthrough.active_nvidia_categories.clone();
                    let gpu_device = self.draft.passthrough.nvidia_gpu_device.clone();
                    let mode = self.draft.passthrough.nvidia_passthrough_mode.clone();
                    let saved_dests: Vec<_> = self
                        .draft
                        .passthrough
                        .nvidia_category_destinations
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let inject_env = self.draft.passthrough.nvidia_inject_env;

                    let mut dialog =
                        crate::tui::widgets::dialogs::nvidia_config::NvidiaConfigDialog::new(
                            gpu_devices,
                            active_cats.clone(),
                            |r| AppMessage::Wizard(WizardMessage::NvidiaConfigSaved(r)),
                        )
                        .with_profile(
                            &gpu_device,
                            &mode,
                            &saved_dests,
                            inject_env,
                            active_cats,
                        );
                    dialog.set_focus(true);
                    StepAction::OpenDialog(Box::new(dialog))
                }
                WizardMessage::OpenUnclassifiedEditDialog(idx, ref file) => {
                    let idx = *idx;
                    let file = file.clone();
                    let mut dialog =
                        crate::tui::widgets::dialogs::unclassified_file::UnclassifiedFileDialog::new(
                            file,
                            move |updated| {
                                AppMessage::Wizard(WizardMessage::UnclassifiedFileUpdated(
                                    idx, updated,
                                ))
                            },
                        );
                    dialog.set_focus(true);
                    StepAction::OpenDialog(Box::new(dialog))
                }
                WizardMessage::NvidiaConfigSaved(ref result) => {
                    let p = &mut self.draft.passthrough;
                    p.nvidia_gpu = true;
                    p.nvidia_gpu_device = result.gpu_device.clone();
                    p.nvidia_passthrough_mode = result.mode.clone();
                    p.nvidia_category_destinations.clear();
                    for (cat, dest) in &result.category_destinations {
                        p.nvidia_category_destinations
                            .insert(cat.clone(), dest.clone());
                    }
                    p.nvidia_inject_env = result.inject_env;
                    if let Some(view) = &mut self.active_view {
                        view.handle_message(&msg)
                    } else {
                        StepAction::None
                    }
                }
                WizardMessage::UserAdded(_)
                | WizardMessage::UserUpdated(_, _)
                | WizardMessage::PortForwardAdded(_)
                | WizardMessage::PortForwardUpdated(_, _)
                | WizardMessage::BindMountAdded(_)
                | WizardMessage::BindMountUpdated(_, _)
                | WizardMessage::UnclassifiedFileUpdated(_, _)
                | WizardMessage::UnresolvedDeploymentReleaseFinished { .. }
                | WizardMessage::DialogCancel => {
                    if let Some(view) = &mut self.active_view {
                        view.handle_message(&msg)
                    } else {
                        StepAction::None
                    }
                }
                _ => StepAction::None,
            },

            _ => StepAction::None,
        }
    }

    pub fn finish_interface_validation(
        &mut self,
        result: Result<
            crate::application::provisioning::InterfaceValidation,
            crate::application::provisioning::DeploymentError,
        >,
    ) -> StepAction {
        self.loading = false;
        match result {
            Ok(crate::application::provisioning::InterfaceValidation::Valid) => {
                self.move_next();
                StepAction::None
            }
            Ok(crate::application::provisioning::InterfaceValidation::Warning(warning)) => {
                self.move_next();
                StepAction::Status(format!("Warning: {warning}"), crate::tui::StatusLevel::Warn)
            }
            Err(error) => StepAction::Status(
                format!("Interface validation failed: {error}"),
                crate::tui::StatusLevel::Error,
            ),
        }
    }

    pub fn finish_hardware_discovery(
        &mut self,
        result: Result<
            crate::application::provisioning::HostHardwareSnapshot,
            crate::application::provisioning::DeploymentError,
        >,
    ) -> StepAction {
        self.draft.passthrough.hardware_scanning = false;
        match result {
            Ok(snapshot) => {
                let warnings = snapshot.warnings.clone();
                self.draft.update_hardware_data(snapshot);
                if matches!(
                    self.step,
                    crate::tui::wizard::WizardStep::HostIntegration
                        | crate::tui::wizard::WizardStep::BindMounts
                ) {
                    self.active_view = None;
                }
                if warnings.is_empty() {
                    StepAction::Status(
                        "Hardware discovery complete".into(),
                        crate::tui::StatusLevel::Info,
                    )
                } else {
                    StepAction::Status(
                        format!(
                            "Hardware discovery completed with warnings: {}",
                            warnings.join("; ")
                        ),
                        crate::tui::StatusLevel::Warn,
                    )
                }
            }
            Err(error) => StepAction::Status(
                format!("Hardware discovery failed: {error}"),
                crate::tui::StatusLevel::Error,
            ),
        }
    }

    pub fn handle_action(&mut self, action: StepAction) -> StepAction {
        match action {
            StepAction::Next => {
                if let Some(view) = &mut self.active_view {
                    if let Err(e) = view.validate() {
                        return StepAction::Status(e, crate::tui::StatusLevel::Error);
                    }
                    view.commit_to_draft(&mut self.draft);
                }

                // Trigger backend validation for network modes with interfaces
                if self.step == WizardStep::Network {
                    let mode = self.draft.network.network_mode();
                    let (name, is_bridge) = match mode {
                        Some(crate::nspawn::models::NetworkMode::Bridge(n)) => (Some(n), true),
                        Some(crate::nspawn::models::NetworkMode::MacVlan(n))
                        | Some(crate::nspawn::models::NetworkMode::IpVlan(n))
                        | Some(crate::nspawn::models::NetworkMode::Interface(n)) => {
                            (Some(n), false)
                        }
                        _ => (None, false),
                    };

                    if let Some(name) = name {
                        self.loading = true;
                        return StepAction::ValidateInterface {
                            name,
                            is_bridge_mode: is_bridge,
                        };
                    }
                }

                self.move_next();
                StepAction::None
            }
            StepAction::Prev => {
                if let Some(view) = &mut self.active_view {
                    // Try to save HEARTBEAT data, but don't block navigation if invalid
                    if view.validate().is_ok() {
                        view.commit_to_draft(&mut self.draft);
                    }
                }
                self.move_prev();
                StepAction::None
            }
            _ => action,
        }
    }

    fn submit_config(&mut self) -> StepAction {
        if let Some(error) = source_permission_error(&self.draft.source.kind, self.permission_level)
        {
            return StepAction::Status(
                format!("Validation Error: {error}"),
                crate::tui::StatusLevel::Error,
            );
        }
        if self.draft.source.kind == SourceKind::Copy {
            let source_cfg = self.draft.source.clone_source.clone();
            if !self
                .draft
                .images
                .iter()
                .any(|image| image.name == source_cfg)
            {
                return StepAction::Status(
                    format!(
                        "Validation Error: Source image '{}' no longer exists",
                        source_cfg
                    ),
                    crate::tui::StatusLevel::Error,
                );
            }
        }
        let target_name = self.draft.basic.name.clone();
        if self
            .draft
            .entries
            .iter()
            .any(|entry| entry.name == target_name)
            || self
                .draft
                .images
                .iter()
                .any(|image| image.name == target_name)
        {
            return StepAction::Status(
                format!(
                    "Validation Error: Container '{}' already exists",
                    target_name
                ),
                crate::tui::StatusLevel::Error,
            );
        }

        self.loading = true;
        let request = self.draft.build_deployment_request();
        self.pending_request = Some(request.clone());
        StepAction::PreflightDeployment(request)
    }

    pub fn finish_preflight(
        &mut self,
        request: DeploymentRequest,
        result: Result<DeploymentPreflight, DeploymentError>,
    ) -> StepAction {
        self.loading = false;
        if self.pending_request.as_ref() != Some(&request) {
            return StepAction::Status(
                "Ignored a stale deployment preflight result".into(),
                crate::tui::StatusLevel::Warn,
            );
        }

        match result {
            Ok(DeploymentPreflight::Ready) => {
                let request = self
                    .pending_request
                    .take()
                    .expect("matched deployment preflight must have a pending request");
                StepAction::StartDeployment(self.draft.take_submission(request))
            }
            Ok(DeploymentPreflight::ConfirmationRequired(risk)) => {
                self.pending_request = None;
                StepAction::OpenDialog(Box::new(TarRiskConfirmationDialog::new(risk)))
            }
            Err(error) => {
                self.pending_request = None;
                StepAction::Status(
                    format!("Deployment preflight failed: {error}"),
                    crate::tui::StatusLevel::Error,
                )
            }
        }
    }

    pub fn start_deployment(&mut self, handle: DeploymentJobHandle) {
        self.loading = false;
        self.step = WizardStep::Deploy;
        self.active_view = Some(Box::new(
            crate::tui::wizard::steps::deploy_view::DeployStepView::new(handle),
        ));
    }

    pub fn preflight_dispatch_failed(&mut self) {
        self.loading = false;
        self.pending_request = None;
    }

    fn move_next(&mut self) {
        if let Some(next_step) = self.resolve_next_step(self.step) {
            self.step = next_step;
            // Evict view so it's recreated with fresh context
            self.active_view = None;
        }
    }

    fn move_prev(&mut self) {
        if let Some(prev_step) = self.resolve_prev_step(self.step) {
            self.step = prev_step;
            self.active_view = None;
        }
    }
}

fn source_permission_error(
    source: &SourceKind,
    permission: crate::composition::PermissionLevel,
) -> Option<&'static str> {
    if matches!(source, SourceKind::Oci) && !permission.is_elevated() {
        Some(
            "OCI imports require root or lasper -e so the generated runtime configuration can be preserved under /etc/systemd/nspawn",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{source_permission_error, TarRiskConfirmationDialog};
    use crate::composition::PermissionLevel;
    use crate::tui::core::{AppMessage, Component, EventResult, WizardMessage};
    use crate::tui::wizard::core::draft::SourceKind;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn oci_submission_requires_root_or_elevated_daemon() {
        assert!(source_permission_error(&SourceKind::Oci, PermissionLevel::User).is_some());
        assert!(source_permission_error(&SourceKind::Oci, PermissionLevel::Elevated).is_none());
        assert!(source_permission_error(&SourceKind::Oci, PermissionLevel::Root).is_none());
    }

    #[test]
    fn regular_sources_keep_existing_permission_policy() {
        assert!(source_permission_error(&SourceKind::Pacstrap, PermissionLevel::User).is_none());
        assert!(source_permission_error(&SourceKind::Copy, PermissionLevel::User).is_none());
    }

    #[test]
    fn tar_risk_dialog_requires_an_explicit_choice() {
        let mut dialog = TarRiskConfirmationDialog::new("GNU tar 1.34 is risky".into());
        assert!(matches!(
            dialog.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            EventResult::Ignored
        ));
        assert!(matches!(
            dialog.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            EventResult::Message(AppMessage::Wizard(WizardMessage::AcceptUnsafeRemoteTar))
        ));
        assert!(matches!(
            dialog.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            EventResult::Message(AppMessage::Wizard(WizardMessage::DeclineUnsafeRemoteTar))
        ));
    }
}
