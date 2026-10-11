use super::make_app;
use crate::application::configuration::ConfigurationTarget;
use crate::application::provisioning::HostHardwareSnapshot;
use crate::domain::machine::MachineName;
use crate::domain::wayland::{HostWaylandSocket, SocketRevision, WaylandDisplay};
use crate::tui::app::{App, ModalLayer};
use crate::tui::features::configuration::ConfigurationView;
use crate::tui::features::wizard::workflow::{WizardCompletion, WizardEvent};
use crate::tui::features::wizard::{Wizard, WizardInstanceId, WizardSession, WizardStep};
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::protocol::events::AppEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{backend::TestBackend, layout::Rect, Terminal};

fn app_with_wizard() -> App {
    let mut app = make_app();
    let mut wizard = Wizard::new(
        WizardInstanceId::new(1),
        crate::tui::features::wizard::draft::WizardDraft::new(
            Vec::new(),
            Vec::new(),
            app.config.nvidia.source(),
            Default::default(),
            app.config.bootstrap.resolve(),
        ),
        crate::composition::PermissionLevel::User,
        app.services.provisioning_preparation.clone(),
    );
    wizard.step = WizardStep::User;
    wizard.draft.passthrough.wayland_sockets = vec![HostWaylandSocket::from_verified_parts(
        WaylandDisplay::new("wayland-0").unwrap(),
        "/run/user/1001".into(),
        "/run/user/1001/wayland-0".into(),
        1001,
        1001,
        1001,
        0o700,
        SocketRevision {
            device: 1,
            inode: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
    )
    .unwrap()];
    app.ui.overlays.wizard = Some(WizardSession::new(wizard));
    app
}

async fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .await;
}

#[tokio::test]
async fn opening_wizard_explains_ignored_profiles_without_blocking_valid_sources() {
    use crate::application::provisioning::{
        DeploymentError, DeploymentRequest, ImagePartitionProbe, InterfaceValidation,
        ProvisioningHostSnapshot, ProvisioningPreparationPort, ProvisioningPreparationService,
    };
    use crate::composition::DefaultPermissionManager;
    use std::sync::Arc;

    struct Preparation;
    #[async_trait::async_trait]
    impl ProvisioningPreparationPort for Preparation {
        async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
            Ok(ProvisioningHostSnapshot::default())
        }
        async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
            std::future::pending().await
        }
        async fn validate_interface(
            &self,
            _: &str,
            _: bool,
        ) -> Result<InterfaceValidation, DeploymentError> {
            unreachable!()
        }
        fn probe_image(
            &self,
            _: &std::path::Path,
        ) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
            unreachable!()
        }
        fn preview(&self, _: &DeploymentRequest) -> String {
            unreachable!()
        }
    }

    let mut app = make_app();
    app.config = Arc::new(
        toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"

                [bootstrap.methods.debootstrap]
                default-profile = "missing"

                [bootstrap.methods.debootstrap.profiles.valid]
                suite = "trixie"

                [bootstrap.methods.dnf5.profiles.incomplete]
                releasever = "43"
            "#,
        )
        .unwrap(),
    );
    app.permissions = Arc::new(DefaultPermissionManager::new().with_elevation(true));
    app.services.provisioning_preparation =
        Arc::new(ProvisioningPreparationService::new(Arc::new(Preparation)));
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    app.ui.app_tx = Some(tx);
    app.begin_wizard();
    let opened = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_event(opened).await;
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Dialog));
    let wizard = app.ui.overlays.wizard.as_ref().unwrap();
    assert_eq!(wizard.step, WizardStep::Source);
    assert_eq!(wizard.draft.source.profiles.len(), 1);
    assert_eq!(wizard.draft.source.profiles[0].name, "valid");

    let mut terminal = Terminal::new(TestBackend::new(110, 35)).unwrap();
    let rows = render(&mut app, &mut terminal);
    assert!(rows
        .iter()
        .any(|row| row.contains("Bootstrap Configuration")));
    assert!(rows.iter().any(|row| row.contains("incomplete")));
    assert!(rows.iter().any(|row| row.contains("\"missing\"")));

    app.handle_event(AppEvent::Wizard(WizardEvent::Completed {
        id: app.ui.overlays.wizard.as_ref().unwrap().id(),
        completion: WizardCompletion::Hardware(Ok(HostHardwareSnapshot {
            nvidia_devices: Vec::new(),
            gpus: Vec::new(),
            active_nvidia_categories: Vec::new(),
            unclassified_nvidia_files: Vec::new(),
            warnings: Vec::new(),
        })),
    }))
    .await;
    let rows = render(&mut app, &mut terminal);
    assert!(rows
        .iter()
        .any(|row| row.contains("Bootstrap Configuration")));

    let (row, label) = rows
        .iter()
        .enumerate()
        .find(|(_, label)| label.contains("Continue"))
        .unwrap();
    let column = unicode_width::UnicodeWidthStr::width(&label[..label.find("Continue").unwrap()]);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: column as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert!(app.ui.overlays.active_dialog.is_none());
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Wizard));
    assert_eq!(
        app.ui.overlays.wizard.as_ref().unwrap().step,
        WizardStep::Source
    );
    let rows = render(&mut app, &mut terminal);
    assert!(!rows
        .iter()
        .any(|row| row.contains("Bootstrap Configuration")));
    app.close_wizard().await;
}

#[tokio::test]
async fn cancelling_wizard_opening_drops_host_inspection_and_ignores_its_late_result() {
    use crate::application::provisioning::{
        DeploymentError, DeploymentRequest, ImagePartitionProbe, InterfaceValidation,
        ProvisioningHostSnapshot, ProvisioningPreparationPort, ProvisioningPreparationService,
    };
    use crate::composition::DefaultPermissionManager;
    use std::sync::Arc;

    struct HostInspection {
        started: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        dropped: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }
    struct InspectionResource(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for InspectionResource {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    #[async_trait::async_trait]
    impl ProvisioningPreparationPort for HostInspection {
        async fn inspect_host(&self) -> Result<ProvisioningHostSnapshot, DeploymentError> {
            let _resource = InspectionResource(self.dropped.lock().take());
            if let Some(tx) = self.started.lock().take() {
                let _ = tx.send(());
            }
            std::future::pending().await
        }
        async fn discover_hardware(&self) -> Result<HostHardwareSnapshot, DeploymentError> {
            panic!("cancelled opening must not start hardware discovery")
        }
        async fn validate_interface(
            &self,
            _: &str,
            _: bool,
        ) -> Result<InterfaceValidation, DeploymentError> {
            unreachable!()
        }
        fn probe_image(
            &self,
            _: &std::path::Path,
        ) -> Result<Option<ImagePartitionProbe>, DeploymentError> {
            unreachable!()
        }
        fn preview(&self, _: &DeploymentRequest) -> String {
            unreachable!()
        }
    }

    let mut app = make_app();
    app.permissions = Arc::new(DefaultPermissionManager::new().with_elevation(true));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    app.services.provisioning_preparation = Arc::new(ProvisioningPreparationService::new(
        Arc::new(HostInspection {
            started: parking_lot::Mutex::new(Some(started_tx)),
            dropped: parking_lot::Mutex::new(Some(dropped_tx)),
        }),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    app.ui.app_tx = Some(tx);
    app.begin_wizard();
    let first_id = app.ui.overlays.wizard_opening.as_ref().unwrap().id();
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Wizard));
    assert!(app.ui.overlays.wizard.is_none());
    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .unwrap()
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    assert!(render(&mut app, &mut terminal)
        .iter()
        .any(|row| row.contains("Inspecting provisioning capabilities")));
    let focus = app.workspace.focus();
    press(&mut app, KeyCode::Tab).await;
    assert_eq!(app.workspace.focus(), focus);
    press(&mut app, KeyCode::Esc).await;
    dropped_rx.await.unwrap();
    assert!(app.ui.overlays.wizard_opening.is_none());
    assert_eq!(app.ui.modal_layer(), None);
    assert!(rx.try_recv().is_err());

    app.begin_wizard();
    let current_id = app.ui.overlays.wizard_opening.as_ref().unwrap().id();
    assert_ne!(first_id, current_id);
    app.handle_event(AppEvent::Wizard(WizardEvent::Opened {
        id: first_id,
        result: Ok(Default::default()),
    }))
    .await;
    assert_eq!(
        app.ui.overlays.wizard_opening.as_ref().unwrap().id(),
        current_id
    );
    assert!(app.ui.overlays.wizard.is_none());
    press(&mut app, KeyCode::Esc).await;
}

#[tokio::test]
async fn leader_survives_pointer_motion_and_scroll_until_a_button_is_pressed() {
    let mut app = make_app();
    app.ui.open_leader();
    let focus = app.workspace.focus();

    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
    ] {
        app.handle_mouse(MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.ui.leader_active(), "leader closed on {kind:?}");
        assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Leader));
        assert_eq!(app.workspace.focus(), focus);
    }

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert!(!app.ui.leader_active());
    assert!(app.ui.overlays.leader.is_none());
}

async fn open_user_editor(app: &mut App) {
    press(app, KeyCode::Tab).await;
    press(app, KeyCode::Char('a')).await;
    assert!(app.ui.overlays.active_dialog.is_some());
}

async fn open_wayland_bindings(app: &mut App) {
    for _ in 0..4 {
        press(app, KeyCode::Tab).await;
    }
    press(app, KeyCode::Char(' ')).await;
}

fn render(app: &mut App, terminal: &mut Terminal<TestBackend>) -> Vec<String> {
    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
    terminal
        .draw(|frame| {
            crate::tui::app::draw(frame, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

#[tokio::test]
async fn wizard_user_editor_and_wayland_dialog_keep_their_layers_and_input_ownership() {
    let mut app = app_with_wizard();
    let screen_area = Rect::new(0, 0, 140, 50);
    let mut terminal =
        Terminal::new(TestBackend::new(screen_area.width, screen_area.height)).unwrap();
    let wizard_area = crate::tui::foundation::geometry::centered_rect(80, 80, screen_area);
    let editor_area = crate::tui::foundation::geometry::centered_rect(70, 85, screen_area);
    let middle = screen_area.height / 2;

    render(&mut app, &mut terminal);
    let wizard_border = terminal.backend().buffer()[(wizard_area.x, middle)].clone();
    open_user_editor(&mut app).await;
    render(&mut app, &mut terminal);
    let editor_border = terminal.backend().buffer()[(editor_area.x, middle)].clone();
    open_wayland_bindings(&mut app).await;
    let rows = render(&mut app, &mut terminal);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(wizard_area.x, middle)], wizard_border);
    assert_eq!(buffer[(editor_area.x, middle)], editor_border);
    assert!(rows.iter().any(|row| row.contains("Add/Edit User")));
    assert!(rows.iter().any(|row| row.contains("Wayland Bindings")));
    assert!(rows.last().unwrap().contains("Space select displays"));
    assert!(rows[..rows.len() - 1]
        .iter()
        .all(|row| !row.contains("Space select displays")));
    assert!(!terminal.backend().cursor_visible());

    // An input intended for the nested dialog must not reach either parent.
    press(&mut app, KeyCode::Char('q')).await;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: wizard_area.x,
        row: middle,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Dialog));
    assert_eq!(
        app.ui.overlays.wizard.as_ref().unwrap().step,
        WizardStep::User
    );

    press(&mut app, KeyCode::Esc).await;
    let rows = render(&mut app, &mut terminal);
    assert!(app.ui.overlays.active_dialog.is_some());
    assert!(rows.iter().any(|row| row.contains("Add/Edit User")));
    assert!(!rows.iter().any(|row| row.contains("Wayland Bindings")));
    assert!(rows.last().unwrap().contains("Esc cancel"));

    press(&mut app, KeyCode::Esc).await;
    assert!(app.ui.overlays.active_dialog.is_none());
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Wizard));
    assert_eq!(
        app.ui.overlays.wizard.as_ref().unwrap().step,
        WizardStep::User
    );
}

#[tokio::test]
async fn hardware_discovery_messages_are_visible_during_wizard_and_nested_dialogs() {
    let mut app = app_with_wizard();
    let mut terminal = Terminal::new(TestBackend::new(140, 50)).unwrap();
    for nested in [false, true] {
        if nested {
            open_user_editor(&mut app).await;
            open_wayland_bindings(&mut app).await;
        }
        app.handle_event(AppEvent::Wizard(WizardEvent::Completed {
            id: WizardInstanceId::new(1),
            completion: WizardCompletion::Hardware(Ok(HostHardwareSnapshot {
                nvidia_devices: Vec::new(),
                gpus: Vec::new(),
                active_nvidia_categories: Vec::new(),
                unclassified_nvidia_files: Vec::new(),
                warnings: Vec::new(),
            })),
        }))
        .await;
        let rows = render(&mut app, &mut terminal);
        assert!(rows.last().unwrap().contains("Hardware discovery complete"));
    }

    app.ui.status_expiry = Some(std::time::Instant::now());
    app.tick().await;
    let rows = render(&mut app, &mut terminal);
    assert!(app.ui.status_message.is_none());
    assert!(rows.last().unwrap().contains("Space select displays"));
}

#[tokio::test]
async fn covered_wizard_inputs_do_not_claim_the_cursor_and_resume_before_redraw() {
    let mut app = app_with_wizard();
    let mut terminal = Terminal::new(TestBackend::new(140, 50)).unwrap();
    render(&mut app, &mut terminal);
    assert!(terminal.backend().cursor_visible());

    app.ui.overlays.active_dialog = Some(Box::new(ConfirmationDialog::new("Confirm", "Continue?")));
    render(&mut app, &mut terminal);
    assert!(!terminal.backend().cursor_visible());

    press(&mut app, KeyCode::Esc).await;
    press(&mut app, KeyCode::Char('x')).await;
    assert_eq!(
        app.ui
            .overlays
            .wizard
            .as_ref()
            .unwrap()
            .draft
            .user
            .root_password,
        "x"
    );
    render(&mut app, &mut terminal);
    assert!(terminal.backend().cursor_visible());
}

#[tokio::test]
async fn status_overrides_the_registered_footer_and_blocks_its_pointer_actions() {
    let mut app = make_app();
    app.ui.overlays.configuration = Some(ConfigurationView::new(ConfigurationTarget::Machine(
        MachineName::new("machine").unwrap(),
    )));
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    let rows = render(&mut app, &mut terminal);
    assert!(rows.last().unwrap().contains("Esc Close"));
    assert!(rows.last().unwrap().starts_with('r'));
    assert!(rows.last().unwrap().contains("? Help"));
    assert!(rows[..rows.len() - 1]
        .iter()
        .all(|row| !row.contains("Esc Close")));
    let close = app.ui.footer_hits.bounds(KeyCode::Esc);
    let click_close = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: close.x,
        row: close.y,
        modifiers: KeyModifiers::NONE,
    };

    app.set_status(
        "Operation complete".into(),
        crate::tui::foundation::status::StatusLevel::Success,
    );
    let rows = render(&mut app, &mut terminal);
    assert!(rows.last().unwrap().contains("Operation complete"));
    assert!(!rows.last().unwrap().contains("Esc Close"));
    app.handle_mouse(click_close).await;
    assert!(app.ui.overlays.configuration.is_some());

    app.ui.status_expiry = Some(std::time::Instant::now());
    app.tick().await;
    let rows = render(&mut app, &mut terminal);
    assert!(rows.last().unwrap().contains("Esc Close"));
    app.handle_mouse(click_close).await;
    assert!(app.ui.overlays.configuration.is_none());
}

#[tokio::test]
async fn wizard_footer_tracks_text_focus_and_available_user_actions() {
    let mut app = app_with_wizard();
    let mut terminal = Terminal::new(TestBackend::new(180, 50)).unwrap();
    let rows = render(&mut app, &mut terminal);
    let footer = rows.last().unwrap();
    assert!(!footer.contains("add user"));
    assert!(!footer.contains("close wizard"));
    press(&mut app, KeyCode::Char('q')).await;
    assert!(app.ui.overlays.wizard.is_some());

    press(&mut app, KeyCode::Tab).await;
    let rows = render(&mut app, &mut terminal);
    let footer = rows.last().unwrap();
    assert!(footer.contains("add user"));
    assert!(!footer.contains("edit user"));
    assert!(!footer.contains("d delete"));

    press(&mut app, KeyCode::Char('a')).await;
    for character in "alice".chars() {
        press(&mut app, KeyCode::Char(character)).await;
    }
    press(&mut app, KeyCode::Enter).await;
    assert!(app.ui.overlays.active_dialog.is_none());
    let rows = render(&mut app, &mut terminal);
    let footer = rows.last().unwrap();
    assert!(footer.contains("edit user"));
    assert!(footer.contains("d delete"));

    press(&mut app, KeyCode::Char('d')).await;
    let rows = render(&mut app, &mut terminal);
    let footer = rows.last().unwrap();
    assert!(!footer.contains("edit user"));
    assert!(!footer.contains("d delete"));
}

#[tokio::test]
async fn covered_source_fields_keep_focus_without_reclaiming_the_cursor() {
    let mut app = app_with_wizard();
    let wizard = app.ui.overlays.wizard.as_mut().unwrap();
    wizard.step = WizardStep::Source;
    wizard.draft.source.kind = crate::tui::features::wizard::draft::SourceKind::LocalFile;
    let mut terminal = Terminal::new(TestBackend::new(140, 50)).unwrap();
    render(&mut app, &mut terminal);
    press(&mut app, KeyCode::Tab).await;
    render(&mut app, &mut terminal);
    assert!(terminal.backend().cursor_visible());

    app.ui.overlays.active_dialog = Some(Box::new(ConfirmationDialog::new("Confirm", "Continue?")));
    render(&mut app, &mut terminal);
    assert!(!terminal.backend().cursor_visible());
    press(&mut app, KeyCode::Esc).await;
    press(&mut app, KeyCode::Char('x')).await;
    assert_eq!(
        app.ui
            .overlays
            .wizard
            .as_ref()
            .unwrap()
            .draft
            .source
            .local_path,
        "x"
    );
}

#[tokio::test]
async fn workspace_footer_clicks_follow_the_same_route_into_and_out_of_overlays() {
    let mut app = make_app();
    let mut terminal = Terminal::new(TestBackend::new(180, 40)).unwrap();
    render(&mut app, &mut terminal);
    let help = app.ui.footer_hits.bounds(KeyCode::Char('?'));
    assert!(help.width > 0);
    let mut click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: help.x,
        row: help.y,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click).await;
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Help));

    render(&mut app, &mut terminal);
    let close = app.ui.footer_hits.bounds(KeyCode::Esc);
    assert!(close.width > 0);
    click.column = close.x;
    click.row = close.y;
    app.handle_mouse(click).await;
    assert_eq!(app.ui.modal_layer(), None);
}

#[tokio::test]
async fn help_scrolls_without_forwarding_input_and_reopens_at_the_top() {
    let mut app = make_app();
    app.ui.overlays.configuration = Some(ConfigurationView::new(ConfigurationTarget::Machine(
        MachineName::new("machine").unwrap(),
    )));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    press(&mut app, KeyCode::Char('?')).await;
    let top = render(&mut app, &mut terminal);
    assert!(top.last().unwrap().contains("PgUp/PgDn page"));

    for key in [KeyCode::PageDown, KeyCode::Down, KeyCode::Char('j')] {
        press(&mut app, key).await;
        assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Help));
    }
    let scrolled = render(&mut app, &mut terminal);
    assert_ne!(scrolled, top);
    press(&mut app, KeyCode::Home).await;
    assert_eq!(render(&mut app, &mut terminal), top);

    let help_area =
        crate::tui::foundation::geometry::centered_rect(60, 85, Rect::new(0, 0, 120, 23));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: help_area.x + 2,
        row: help_area.y + 2,
        modifiers: KeyModifiers::NONE,
    })
    .await;
    assert_ne!(render(&mut app, &mut terminal), top);
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Help));
    assert!(app.ui.overlays.configuration.is_some());

    press(&mut app, KeyCode::Char('q')).await;
    assert!(!app.should_quit);
    assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Configuration));
    press(&mut app, KeyCode::Char('?')).await;
    assert_eq!(render(&mut app, &mut terminal), top);
}
