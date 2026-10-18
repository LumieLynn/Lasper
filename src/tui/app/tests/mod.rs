#![cfg(test)]

mod overlays;

use crate::application::{ImageLifecycleService, MachineLifecycleService};
use crate::domain::inspection::{MachineProperties, GROUP_SYSTEMD_UNIT};
use crate::domain::runtime::RuntimeSnapshot;
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry, MachineState};
use crate::tui::app::{App, AppUi, ModalLayer};
use crate::tui::features::wizard::workflow::{WizardCompletion, WizardEvent};
use crate::tui::features::workspace::inspector::DetailTarget;
use crate::tui::features::workspace::resources::actions::machine_action_can_change_x11_lifecycle;
use crate::tui::features::workspace::resources::actions::PendingImageRemoval;
use crate::tui::features::workspace::WorkspaceFocus;
use crate::tui::protocol::events::AppEvent;
use crate::tui::protocol::Component;
use anyhow::Result;
use std::time::Duration;

#[test]
fn production_app_state_has_no_execution_context_or_adapter_store() {
    let production = include_str!("../core/state.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("application module has a production section");

    assert!(!production.contains("ExecutionContext"));
    assert!(!production.contains("crate::adapters"));
}

#[test]
fn only_machine_runtime_actions_schedule_x11_lifecycle_reconcile() {
    use crate::application::{MachineLifecycleAction, MachineRuntimeAction};

    assert!(machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Runtime(MachineRuntimeAction::Poweroff)
    ));
    assert!(machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Runtime(MachineRuntimeAction::Terminate)
    ));
    assert!(machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Runtime(MachineRuntimeAction::Reboot)
    ));
    assert!(machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Runtime(MachineRuntimeAction::Kill {
            signal: crate::domain::machine::AllowedSignal::Kill,
        })
    ));
    assert!(!machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Launch
    ));
    assert!(!machine_action_can_change_x11_lifecycle(
        MachineLifecycleAction::Unit(crate::application::NspawnUnitAction::Enable)
    ));
}

fn make_entry(name: &str, state: MachineState) -> MachineEntry {
    MachineEntry {
        name: name.to_string(),
        class: MachineEntry::NSPAWN_CLASS.into(),
        service: MachineEntry::NSPAWN_SERVICE.into(),
        state,
        addresses: Default::default(),
    }
}

fn make_image(name: &str) -> ImageEntry {
    ImageEntry {
        name: name.to_string(),
        image_type: "directory".to_string(),
        readonly: false,
        usage: None,
        dbus_object_path: None,
    }
}

fn make_internal_image(name: &str) -> ImageEntry {
    make_image(name)
}

fn make_app() -> App {
    let permissions = std::sync::Arc::new(crate::composition::DefaultPermissionManager::new());
    let mode =
        crate::composition::CompositionMode::new(crate::composition::PermissionLevel::User, None)
            .unwrap();
    let services = crate::composition::compose_application_services(
        mode,
        false,
        crate::domain::nvidia::NvidiaCdiSource::default(),
    );
    App::new(
        permissions,
        false, // systemd_tools
        0,
        services,
        std::sync::Arc::new(crate::config::AppConfig::default()),
    )
}

#[tokio::test]
async fn configure_leader_uses_each_focused_resource_and_blocks_workspace_input() {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    for focus in [
        WorkspaceFocus::Machines,
        WorkspaceFocus::Images,
        WorkspaceFocus::MachineInspector,
        WorkspaceFocus::ImageInspector,
    ] {
        let mut app = make_app();
        app.workspace.resources.catalog.entries =
            vec![make_entry("running-machine", MachineState::Running)];
        app.workspace.resources.catalog.images = vec![make_image("image with spaces")];
        app.workspace.set_focus(focus, None);
        app.workspace.inspector.state.target = match focus {
            WorkspaceFocus::ImageInspector => DetailTarget::Image {
                name: "image with spaces".into(),
                internal: false,
            },
            _ => DetailTarget::Machine("running-machine".into()),
        };
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .await;
        let expected = if matches!(
            focus,
            WorkspaceFocus::Images | WorkspaceFocus::ImageInspector
        ) {
            "image with spaces"
        } else {
            "running-machine"
        };
        assert_eq!(
            app.ui
                .overlays
                .configuration
                .as_ref()
                .unwrap()
                .target
                .name(),
            expected
        );
        assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Configuration));
        assert!(app.ui.overlays.leader.is_none());
        for key in ['t', 'n', 'q', ' ', ']'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .await;
        }
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.workspace.focus(), focus);
        assert!(!app.should_quit);
        assert!(app.ui.overlays.leader.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.ui.overlays.configuration.is_none());
        assert_eq!(app.workspace.focus(), focus);
    }
}

#[tokio::test]
async fn configure_leader_does_not_open_for_internal_images() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    for focus in [WorkspaceFocus::Images, WorkspaceFocus::ImageInspector] {
        let mut app = make_app();
        app.workspace.resources.catalog.internal_images = vec![make_internal_image(".internal")];
        app.workspace.set_focus(focus, None);
        app.workspace.inspector.state.target = DetailTarget::Image {
            name: ".internal".into(),
            internal: true,
        };
        app.workspace.resources.handle_key(
            crate::tui::features::workspace::resources::catalog::ResourceListKind::InternalImages,
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
        );

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .await;

        assert!(app.ui.overlays.configuration.is_none());
        assert!(app.ui.overlays.leader.is_none());
    }
}

#[tokio::test]
async fn configure_menu_keeps_its_original_target_after_catalog_selection_changes() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = make_app();
    let original = make_image("source-image");
    let mut menu = crate::tui::features::workspace::resources::menu::ResourceActionMenu::for_image(
        &original, false, false,
    );
    for _ in 0..4 {
        menu.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    app.ui.overlays.resource_action_menu = Some(menu);
    app.workspace.resources.catalog.images = vec![make_image("different-image")];
    app.workspace.set_focus(WorkspaceFocus::Images, None);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(
        app.ui
            .overlays
            .configuration
            .as_ref()
            .unwrap()
            .target
            .name(),
        "source-image"
    );
    assert!(app.ui.overlays.resource_action_menu.is_none());
}

#[tokio::test]
async fn stale_background_result_cannot_mutate_a_reopened_wizard() {
    let mut app = make_app();
    let current_id = crate::tui::features::wizard::WizardInstanceId::new(2);
    app.ui.overlays.wizard = Some(crate::tui::features::wizard::WizardSession::new(
        crate::tui::features::wizard::Wizard::new(
            current_id,
            crate::tui::features::wizard::draft::WizardDraft::new(
                Vec::new(),
                Vec::new(),
                app.config.nvidia.source(),
                Default::default(),
                app.config.bootstrap.resolve(),
            ),
            crate::composition::PermissionLevel::User,
            app.services.provisioning_preparation.clone(),
        ),
    ));
    assert!(
        app.ui
            .overlays
            .wizard
            .as_ref()
            .unwrap()
            .draft
            .passthrough
            .hardware_scanning
    );

    app.handle_event(AppEvent::Wizard(WizardEvent::Completed {
        id: crate::tui::features::wizard::WizardInstanceId::new(1),
        completion: WizardCompletion::Hardware(Err(
            crate::application::provisioning::DeploymentError::failed("stale result"),
        )),
    }))
    .await;

    let wizard = app.ui.overlays.wizard.as_ref().unwrap();
    assert_eq!(wizard.id(), current_id);
    assert!(wizard.draft.passthrough.hardware_scanning);
    assert!(app.ui.status_message.is_none());
}

mod image_start_transitions {
    use super::*;
    use crate::application::machine_lifecycle::{
        MachineControlOutcome, MockMachineControl, MockMachineObservation,
        MockMachineStartDiagnostics, MockMachineStartPreparation, RoutedMachineControlOutcome,
    };
    use crate::application::operations::route::ExecutionRoute;
    use crate::application::{MachineLifecycleResult, OperationRegistry};
    use crate::tui::protocol::events::AppEvent;

    fn prepare_image_start(
        control_outcome: MachineControlOutcome,
    ) -> (App, tokio::sync::mpsc::Receiver<AppEvent>) {
        let successful = matches!(control_outcome, MachineControlOutcome::Succeeded);
        let mut control = MockMachineControl::new();
        control
            .expect_launch()
            .once()
            .returning(move |_, _| RoutedMachineControlOutcome {
                outcome: control_outcome.clone(),
                route: ExecutionRoute::DirectDbus,
                fallback: None,
            });
        let mut preparation = MockMachineStartPreparation::new();
        preparation.expect_prepare().once().returning(|_| Ok(()));
        let mut observation = MockMachineObservation::new();
        if successful {
            observation.expect_inspect().once().returning(|_, _| {
                let mut properties = MachineProperties::default();
                properties.insert(GROUP_SYSTEMD_UNIT, "ActiveState".into(), "active".into());
                Ok(properties)
            });
        }
        observation.expect_invalidate().once().return_const(());
        let diagnostics = MockMachineStartDiagnostics::new();
        let lifecycle = std::sync::Arc::new(MachineLifecycleService::new(
            std::sync::Arc::new(control),
            std::sync::Arc::new(preparation),
            std::sync::Arc::new(observation),
            std::sync::Arc::new(diagnostics),
            OperationRegistry::new(),
        ));
        let mut app = make_app();
        app.services.machine_lifecycle = lifecycle;
        app.workspace.resources.catalog.images = vec![make_image("test")];
        app.workspace.set_focus(WorkspaceFocus::Images, None);
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        app.ui.app_tx = Some(tx);
        (app, rx)
    }

    #[tokio::test]
    async fn image_start_adds_machine_before_backend_completion() {
        let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);

        app.action_start();

        assert_eq!(
            app.workspace.resources.catalog.entries,
            vec![MachineEntry::optimistic_nspawn(
                "test",
                MachineState::Starting
            )]
        );

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("start task should finish")
            .expect("start task should report a result");
        tokio::task::yield_now().await;
        assert_eq!(app.services.host_operations.active_count(), 0);
        assert!(matches!(
            event,
            AppEvent::MachineOperationFinished(outcome)
                if outcome.result == MachineLifecycleResult::Succeeded
        ));

        app.sync_snapshot(RuntimeSnapshot::new(
            vec![make_entry("test", MachineState::Running)],
            vec![make_image("test")],
        ));
        assert_eq!(
            app.workspace.resources.catalog.entries,
            vec![make_entry("test", MachineState::Running)]
        );
    }

    #[tokio::test]
    async fn image_start_failure_removes_synthetic_machine() {
        let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Failed {
            reason: "start rejected".into(),
        });

        app.action_start();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("start task should finish")
            .expect("start task should report a result");
        let AppEvent::MachineOperationFinished(outcome) = event else {
            panic!("start failure should report a semantic outcome");
        };
        assert_eq!(
            outcome.result,
            MachineLifecycleResult::Failed("start rejected".into())
        );
        app.handle_machine_operation_result(outcome);
        assert!(app.workspace.resources.catalog.entries.is_empty());
    }

    #[tokio::test]
    async fn image_start_does_not_dispatch_again_while_machine_is_starting() {
        let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);

        app.action_start();
        app.action_start();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first start task should finish")
            .expect("first start task should report a result");
        assert!(matches!(event, AppEvent::MachineOperationFinished(..)));
        assert_eq!(
            app.ui
                .status_message
                .as_ref()
                .map(|(message, _)| message.as_str()),
            Some("test is already starting.")
        );
    }

    #[tokio::test]
    async fn image_inspector_action_menu_keeps_its_retained_target() {
        let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);
        app.workspace
            .resources
            .catalog
            .images
            .push(make_image("second"));
        app.workspace.resources.catalog.image_selected = 1;
        app.workspace.inspector.state.target = DetailTarget::Image {
            name: "test".into(),
            internal: false,
        };
        app.workspace
            .set_focus(WorkspaceFocus::ImageInspector, None);

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .await;

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
        .await;

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("start task should finish")
            .expect("start task should report a result");
        let AppEvent::MachineOperationFinished(outcome) = event else {
            panic!("start should report a machine lifecycle outcome");
        };
        assert_eq!(outcome.machine.as_str(), "test");
    }

    #[tokio::test]
    async fn mstack_start_warns_but_still_dispatches_to_systemd() {
        let (mut app, mut rx) = prepare_image_start(MachineControlOutcome::Succeeded);
        app.workspace.resources.catalog.images[0].image_type = "mstack".into();

        app.action_start();

        assert_eq!(
            app.workspace.resources.catalog.entries[0].state,
            MachineState::Starting
        );
        assert_eq!(
            app.ui
                .status_message
                .as_ref()
                .map(|(message, _)| message.as_str()),
            Some(
                "test is an OCI application and may not be bootable; systemd will still try to start it."
            )
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("mstack start should finish")
                .expect("mstack start should report a result"),
            AppEvent::MachineOperationFinished(..)
        ));
    }
}

mod image_removal {
    use super::*;
    use crate::application::image_lifecycle::{
        ArtifactCleanupReport, ImageControlOutcome, MockImageControl, MockImageRuntime,
        MockManagedArtifactCleanup, UnitDisableReport,
    };
    use crate::application::OperationRegistry;
    use crate::tui::protocol::events::AppEvent;

    fn prepare_image_removal(
        control: MockImageControl,
        cleanup: MockManagedArtifactCleanup,
    ) -> (App, tokio::sync::mpsc::Receiver<AppEvent>) {
        let mut runtime = MockImageRuntime::new();
        runtime.expect_list_machines().returning(|| Ok(Vec::new()));
        let mut app = make_app();
        app.services.image_lifecycle = std::sync::Arc::new(ImageLifecycleService::new(
            std::sync::Arc::new(runtime),
            std::sync::Arc::new(control),
            std::sync::Arc::new(cleanup),
            OperationRegistry::new(),
        ));
        app.workspace.resources.catalog.images = vec![make_image("first"), make_image("second")];
        app.workspace.set_focus(WorkspaceFocus::Images, None);
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        app.ui.app_tx = Some(tx);
        (app, rx)
    }

    #[tokio::test]
    async fn confirmation_binds_target_across_selection_and_focus_changes() {
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .withf(|name| name.as_str() == "first")
            .once()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .withf(|name| name.as_str() == "first")
            .once()
            .returning(|_| ImageControlOutcome::Removed);
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup
            .expect_cleanup()
            .withf(|name| name.as_str() == "first")
            .once()
            .returning(|_| ArtifactCleanupReport::Removed);
        let (mut app, mut rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        app.workspace.resources.catalog.image_selected = 1;
        app.workspace.set_focus(WorkspaceFocus::Machines, None);
        app.action_remove();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("remove task should finish")
            .expect("remove task should report a result");
        assert!(matches!(
            event,
            AppEvent::ImageRemovalFinished(completion)
                if completion.status() == ("Removed image first and Lasper artifacts".into(), crate::tui::foundation::status::StatusLevel::Success)
        ));
    }

    #[tokio::test]
    async fn confirmation_can_skip_lasper_artifact_cleanup() {
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .withf(|name| name.as_str() == "first")
            .once()
            .returning(|_| ImageControlOutcome::Removed);
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().never();
        let (mut app, mut rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        let result = app
            .ui
            .overlays
            .delete_dialog
            .as_mut()
            .expect("delete dialog")
            .handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            ));
        assert!(matches!(
            result,
            crate::tui::protocol::EventResult::Consumed
        ));
        app.action_remove();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("remove task should finish")
            .expect("remove task should report a result");
        assert!(matches!(
            event,
            AppEvent::ImageRemovalFinished(completion)
                if completion.status() == ("Removed image first".into(), crate::tui::foundation::status::StatusLevel::Success)
        ));
    }

    #[tokio::test]
    async fn cleanup_failure_preserves_successful_removal_result() {
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .once()
            .returning(|_| ImageControlOutcome::Removed);
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup
            .expect_cleanup()
            .once()
            .returning(|_| ArtifactCleanupReport::Failed(vec!["cleanup failed".into()]));
        let (mut app, mut rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        app.action_remove();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("remove task should finish")
            .expect("remove task should report a result");
        assert!(matches!(
            event,
            AppEvent::ImageRemovalFinished(completion)
                if { let (message, level) = completion.status();
                    level == crate::tui::foundation::status::StatusLevel::Warn
                    && message.contains("Removed image first") && message.contains("cleanup failed") }
        ));
    }

    #[tokio::test]
    async fn ambiguous_artifacts_are_reported_as_preserved() {
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .once()
            .returning(|_| ImageControlOutcome::Removed);
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().once().returning(|_| {
            ArtifactCleanupReport::PreservedAmbiguous(vec!["legacy drop-in was preserved".into()])
        });
        let (mut app, mut rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        app.action_remove();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("remove task should finish")
            .expect("remove task should report a result");
        assert!(matches!(
            event,
            AppEvent::ImageRemovalFinished(completion)
                if { let (message, level) = completion.status();
                    level == crate::tui::foundation::status::StatusLevel::Warn
                    && message.contains("Removed image first") && message.contains("legacy drop-in was preserved") }
        ));
    }

    #[tokio::test]
    async fn removal_failure_never_runs_optional_cleanup() {
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .once()
            .returning(|_| ImageControlOutcome::Failed {
                reason: "remove failed".into(),
            });
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().never();
        let (mut app, mut rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        app.action_remove();

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("remove task should finish")
            .expect("remove task should report a result");
        assert!(matches!(
            event,
            AppEvent::ImageRemovalFinished(completion)
                if completion.status() == ("Remove failed: remove failed".into(), crate::tui::foundation::status::StatusLevel::Error)
        ));
    }

    #[test]
    fn confirmation_rejects_a_target_that_disappeared() {
        let mut control = MockImageControl::new();
        control.expect_disable_unit().never();
        control.expect_remove_image().never();
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().never();
        let (mut app, _rx) = prepare_image_removal(control, cleanup);

        app.show_delete_dialog();
        app.workspace.resources.catalog.images.clear();
        app.action_remove();

        assert_eq!(
            app.ui
                .status_message
                .as_ref()
                .map(|(message, _)| message.as_str()),
            Some("Image first changed before confirmation; refresh and try again.")
        );
    }
}

mod tar_risk_confirmation {
    use super::*;
    use crate::application::provisioning::{
        DeploymentError, DeploymentExecutor, DeploymentJobContext, DeploymentPlan,
        DeploymentPreflight, DeploymentSecrets, ProvisioningService, RemoteTarSafety,
        SourcePreflight,
    };
    use crate::composition::PermissionLevel;
    use crate::tui::features::wizard::{Wizard, WizardStep};
    use crate::tui::protocol::{AppMessage, WizardMessage};
    use async_trait::async_trait;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    struct TarConfirmationPort;

    struct SecretRecordingExecutor {
        accepted: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<DeploymentSecrets>>>,
    }

    #[async_trait]
    impl DeploymentExecutor for SecretRecordingExecutor {
        async fn run(
            &self,
            _plan: DeploymentPlan,
            secrets: DeploymentSecrets,
            _context: DeploymentJobContext,
        ) -> Result<(), DeploymentError> {
            self.accepted.lock().take().unwrap().send(secrets).unwrap();
            Ok(())
        }
    }

    #[async_trait]
    impl SourcePreflight for TarConfirmationPort {
        async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
            Ok(RemoteTarSafety::Risk(
                "GNU tar 1.34 lacks hard-link confinement".into(),
            ))
        }
    }

    #[async_trait]
    impl DeploymentExecutor for TarConfirmationPort {
        async fn run(
            &self,
            _plan: DeploymentPlan,
            _secrets: DeploymentSecrets,
            _context: DeploymentJobContext,
        ) -> Result<(), DeploymentError> {
            std::future::pending().await
        }
    }

    async fn prepare_confirmation() -> (
        App,
        tokio::sync::mpsc::Receiver<crate::tui::protocol::events::AppEvent>,
    ) {
        let mut app = make_app();
        app.services.provisioning = Arc::new(ProvisioningService::new(
            Arc::new(TarConfirmationPort),
            Arc::new(TarConfirmationPort),
            Arc::new(crate::application::provisioning::MemoryDeploymentStatePort::default()),
            Arc::new(crate::application::provisioning::MemoryDeploymentRecoveryProbe),
            Arc::new(crate::application::provisioning::MemoryDeploymentClaimControl),
            crate::application::OperationRegistry::new(),
        ));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        app.ui.app_tx = Some(event_tx);
        let mut wizard = Wizard::new(
            crate::tui::features::wizard::WizardInstanceId::new(1),
            crate::tui::features::wizard::draft::WizardDraft::new(
                Vec::new(),
                Vec::new(),
                app.config.nvidia.source(),
                Default::default(),
                app.config.bootstrap.resolve(),
            ),
            PermissionLevel::User,
            app.services.provisioning_preparation.clone(),
        );
        wizard.step = WizardStep::Review;
        wizard.draft.source.kind = crate::tui::features::wizard::draft::SourceKind::Pull;
        wizard.draft.source.pull_url = "https://example.test/rootfs.tar".into();
        wizard.draft.source.is_pull_raw = false;
        wizard.draft.basic.name = "tar-test".into();
        wizard.draft.user.root_password = "root-secret".into();
        wizard
            .draft
            .user
            .users
            .push(crate::tui::features::wizard::draft::UserDraft {
                username: "alice".into(),
                password: "user-secret".into(),
                sudoer: false,
                shell: "/bin/bash".into(),
                wayland: None,
            });
        wizard.active_view = None;
        app.ui.overlays.wizard = Some(crate::tui::features::wizard::WizardSession::new(wizard));
        let action = app
            .ui
            .overlays
            .wizard
            .as_mut()
            .unwrap()
            .process_message(AppMessage::Wizard(WizardMessage::Submit));
        app.handle_wizard_action(action).await;
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_event(event).await;
        (app, event_rx)
    }

    #[tokio::test]
    async fn rejected_submission_preserves_passwords_for_a_direct_review_retry() {
        let (mut app, mut event_rx) = prepare_confirmation().await;
        let registry = crate::application::OperationRegistry::new();
        let reservation = registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(
                    &crate::domain::machine::MachineName::new("tar-test").unwrap(),
                ),
            )])
            .unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        app.services.provisioning = Arc::new(ProvisioningService::new(
            Arc::new(TarConfirmationPort),
            Arc::new(SecretRecordingExecutor {
                accepted: parking_lot::Mutex::new(Some(accepted_tx)),
            }),
            Arc::new(crate::application::provisioning::MemoryDeploymentStatePort::default()),
            Arc::new(crate::application::provisioning::MemoryDeploymentRecoveryProbe),
            Arc::new(crate::application::provisioning::MemoryDeploymentClaimControl),
            registry,
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .await;
        app.handle_event(event_rx.recv().await.unwrap()).await;
        let wizard = app.ui.overlays.wizard.as_ref().unwrap();
        assert_eq!(wizard.step, WizardStep::Review);
        assert!(!wizard.loading);
        assert_eq!(wizard.draft.user.root_password, "root-secret");
        assert_eq!(wizard.draft.user.users[0].password, "user-secret");
        assert!(app
            .ui
            .status_message
            .as_ref()
            .unwrap()
            .0
            .contains("resource is busy"));

        drop(reservation);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        app.handle_event(event_rx.recv().await.unwrap()).await;
        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().step,
            WizardStep::Deploy
        );
        let mut secrets = accepted_rx.await.unwrap();
        assert_eq!(
            secrets.take_root_password().unwrap().expose_secret(),
            "root-secret"
        );
        assert_eq!(
            secrets
                .take_user_password("alice")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "user-secret"
        );
    }

    #[tokio::test]
    async fn topmost_tar_dialog_consumes_keys_and_decline_never_submits() {
        let (mut app, mut event_rx) = prepare_confirmation().await;
        assert!(app.ui.overlays.active_dialog.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .await;
        assert!(app.ui.overlays.active_dialog.is_some());
        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().step,
            WizardStep::Review
        );
        assert!(event_rx.try_recv().is_err());
        assert_eq!(
            app.ui
                .overlays
                .wizard
                .as_ref()
                .unwrap()
                .draft
                .user
                .root_password,
            "root-secret"
        );
        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().draft.user.users[0].password,
            "user-secret"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .await;
        assert!(app.ui.overlays.active_dialog.is_none());
        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().step,
            WizardStep::Review
        );
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn accepting_tar_risk_submits_once_and_enter_does_not_cancel_deployment() {
        let (mut app, mut event_rx) = prepare_confirmation().await;

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .await;
        assert!(app.ui.overlays.active_dialog.is_none());
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let AppEvent::Wizard(WizardEvent::Completed {
            completion: WizardCompletion::Preflight { request, .. },
            ..
        }) = &event
        else {
            panic!("confirmation must run deployment preflight");
        };
        assert!(request.allow_unsafe_remote_tar);
        app.handle_event(event).await;
        assert!(event_rx.try_recv().is_err());

        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().step,
            WizardStep::Deploy
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        let wizard = app.ui.overlays.wizard.as_ref().unwrap();
        assert_eq!(wizard.step, WizardStep::Deploy);
        assert!(wizard.draft.user.root_password.is_empty());
        assert!(wizard.draft.user.users[0].password.is_empty());
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_preflight_completion_cannot_consume_wizard_secrets() {
        let (mut app, mut event_rx) = prepare_confirmation().await;
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .await;
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let (wizard_id, preflight_id, request) = match &event {
            AppEvent::Wizard(WizardEvent::Completed {
                id,
                completion: WizardCompletion::Preflight { query, request, .. },
            }) => (*id, *query, request.clone()),
            _ => panic!("confirmation must run deployment preflight"),
        };

        app.handle_event(AppEvent::Wizard(WizardEvent::Completed {
            id: wizard_id,
            completion: WizardCompletion::Preflight {
                query: preflight_id.wrapping_add(1),
                request,
                result: Ok(DeploymentPreflight::Ready),
            },
        }))
        .await;

        let wizard = app.ui.overlays.wizard.as_ref().unwrap();
        assert_eq!(wizard.step, WizardStep::Review);
        assert_eq!(wizard.draft.user.root_password, "root-secret");
        assert_eq!(wizard.draft.user.users[0].password, "user-secret");
        assert_eq!(wizard.pending_preflight(), Some(preflight_id));

        app.handle_event(event).await;
        assert_eq!(
            app.ui.overlays.wizard.as_ref().unwrap().step,
            WizardStep::Deploy
        );
    }
}

mod select_next_prev {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn next_wraps() {
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("a", MachineState::Running),
            make_entry("b", MachineState::Running),
            make_entry("c", MachineState::Running),
        ];
        app.workspace.resources.catalog.selected = 2;

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.resources.catalog.selected, 0);
    }

    #[tokio::test]
    async fn prev_wraps() {
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("a", MachineState::Running),
            make_entry("b", MachineState::Running),
            make_entry("c", MachineState::Running),
        ];
        app.workspace.resources.catalog.selected = 0;

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.resources.catalog.selected, 2);
    }

    #[tokio::test]
    async fn next_empty_no_panic() {
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![];
        app.workspace.resources.catalog.selected = 0;

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
    }

    #[tokio::test]
    async fn prev_empty_no_panic() {
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![];
        app.workspace.resources.catalog.selected = 0;

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
    }

    #[tokio::test]
    async fn image_navigation_is_independent_from_machine_selection() {
        let mut app = make_app();
        app.workspace.resources.catalog.entries =
            vec![make_entry("machine", MachineState::Running)];
        app.workspace.resources.catalog.images = vec![make_image("a"), make_image("b")];
        app.workspace.set_focus(WorkspaceFocus::Images, None);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;

        assert_eq!(app.workspace.resources.catalog.image_selected, 1);
        assert_eq!(app.workspace.resources.catalog.selected, 0);
    }

    #[tokio::test]
    async fn internal_image_navigation_has_an_independent_selection() {
        let mut app = make_app();
        app.workspace.resources.catalog.images = vec![make_image("regular")];
        app.workspace.resources.catalog.internal_images = vec![
            make_internal_image(".internal-a"),
            make_internal_image(".internal-b"),
        ];
        app.workspace.set_focus(WorkspaceFocus::Images, None);
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;

        assert_eq!(app.workspace.resources.catalog.internal_image_selected, 1);
        assert_eq!(app.workspace.resources.catalog.image_selected, 0);
    }
}

mod focus_and_modal_input {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn app_with_machine_and_image() -> App {
        let mut app = make_app();
        app.workspace.resources.catalog.entries =
            vec![make_entry("machine", MachineState::Running)];
        app.workspace.resources.catalog.images = vec![make_image("image")];
        app.set_focus(WorkspaceFocus::Machines);
        app
    }

    fn render_detail(
        app: &mut App,
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> String {
        terminal
            .draw(|frame| {
                app.workspace.inspector.render(
                    frame,
                    frame.area(),
                    &app.workspace.resources.catalog,
                    app.workspace.focus().is_inspector(),
                    false,
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn detail_focus_space_enters_a_one_level_leader_keymap() {
        let mut app = app_with_machine_and_image();
        app.set_focus(WorkspaceFocus::MachineInspector);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await;
        assert!(app.ui.leader_active());

        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .await;
        assert!(!app.ui.leader_active());
        assert!(app.ui.overlays.resource_action_menu.is_none());
    }

    #[tokio::test]
    async fn leader_x_opens_an_explicit_host_x11_prompt() {
        let mut app = app_with_machine_and_image();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        app.ui.app_tx = Some(tx);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;

        assert!(!app.ui.leader_active());
        assert_eq!(app.terminal.sessions.len(), 1);
        assert!(app.terminal.is_showing());
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Terminal);
        assert!(app
            .ui
            .status_message
            .as_ref()
            .is_some_and(|(message, _)| message.contains("Host X11 shell prompt")));
        app.terminal.close_active();
    }

    #[tokio::test]
    async fn leader_escape_only_closes_the_transient_keymap() {
        let mut app = app_with_machine_and_image();
        app.set_focus(WorkspaceFocus::MachineInspector);
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;

        assert!(!app.ui.leader_active());
        assert!(app.ui.overlays.active_dialog.is_none());
    }

    #[tokio::test]
    async fn list_focus_enters_the_same_leader_keymap() {
        let mut app = app_with_machine_and_image();
        for focus in [WorkspaceFocus::Machines, WorkspaceFocus::Images] {
            app.set_focus(focus);
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
                .await;
            assert!(app.ui.leader_active());
            assert_eq!(app.ui.modal_layer(), Some(ModalLayer::Leader));
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await;
            assert!(!app.ui.leader_active());
        }
    }

    #[test]
    fn changing_workspace_focus_destroys_the_leader_component() {
        let mut app = app_with_machine_and_image();
        app.set_focus(WorkspaceFocus::Machines);
        app.ui.open_leader();
        assert!(app.ui.leader_active());

        app.set_focus(WorkspaceFocus::Images);
        assert!(!app.ui.leader_active());
        assert!(app.ui.overlays.leader.is_none());
    }

    #[test]
    fn modal_layer_reports_the_input_owner() {
        use crate::tui::foundation::components::confirmation::ConfirmationDialog;

        let app = make_app();
        let mut ui = AppUi::new();
        ui.overlays.resource_action_menu = Some(
            crate::tui::features::workspace::resources::menu::ResourceActionMenu::for_machine(
                &make_entry("machine", MachineState::Running),
            ),
        );
        assert_eq!(ui.modal_layer(), Some(ModalLayer::ResourceActionMenu));

        ui.overlays.wizard = Some(crate::tui::features::wizard::WizardSession::new(
            crate::tui::features::wizard::Wizard::new(
                crate::tui::features::wizard::WizardInstanceId::new(1),
                crate::tui::features::wizard::draft::WizardDraft::new(
                    Vec::new(),
                    Vec::new(),
                    app.config.nvidia.source(),
                    Default::default(),
                    app.config.bootstrap.resolve(),
                ),
                crate::composition::PermissionLevel::User,
                app.services.provisioning_preparation.clone(),
            ),
        ));
        assert_eq!(ui.modal_layer(), Some(ModalLayer::Wizard));
        ui.overlays.open_help();
        assert_eq!(ui.modal_layer(), Some(ModalLayer::Help));

        ui.overlays.quit_dialog = Some(ConfirmationDialog::new("Quit", "Confirm"));
        assert_eq!(ui.modal_layer(), Some(ModalLayer::QuitConfirmation));
        ui.overlays.delete_dialog = Some(PendingImageRemoval::new(
            ImageName::new("image").unwrap(),
            ConfirmationDialog::new("Remove", "Confirm"),
        ));
        assert_eq!(ui.modal_layer(), Some(ModalLayer::DeleteConfirmation));

        ui.overlays.active_dialog = Some(Box::new(ConfirmationDialog::new("Dialog", "Confirm")));
        assert_eq!(ui.modal_layer(), Some(ModalLayer::Dialog));
    }

    #[tokio::test]
    async fn tab_cycle_pairs_each_list_with_its_inspector() {
        let mut app = app_with_machine_and_image();

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::MachineInspector);
        assert_eq!(
            app.workspace.inspector.state.target,
            DetailTarget::Machine("machine".into())
        );

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Images);
        assert!(matches!(
            app.workspace.inspector.state.target,
            DetailTarget::Image { .. }
        ));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::ImageInspector);
        assert!(matches!(
            app.workspace.inspector.state.target,
            DetailTarget::Image { .. }
        ));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Machines);
    }

    #[tokio::test]
    async fn reverse_tab_cycle_is_the_exact_inverse() {
        let mut app = app_with_machine_and_image();

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::ImageInspector);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Images);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::MachineInspector);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Machines);
    }

    #[tokio::test]
    async fn terminal_joins_the_cycle_and_maximized_mode_skips_inspectors() {
        let mut app = app_with_machine_and_image();
        app.terminal.show = true;
        app.set_focus(WorkspaceFocus::ImageInspector);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Terminal);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Machines);

        app.terminal.maximized = true;
        app.set_focus(WorkspaceFocus::Machines);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Images);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Terminal);
    }

    #[test]
    fn focus_restore_tracks_the_latest_non_terminal_panel() {
        let mut app = make_app();
        app.set_focus(WorkspaceFocus::Images);
        app.set_focus(WorkspaceFocus::Terminal);
        assert_eq!(
            app.workspace.restore_non_terminal_focus(),
            WorkspaceFocus::Images
        );

        app.set_focus(WorkspaceFocus::Machines);
        app.set_focus(WorkspaceFocus::Terminal);
        assert_eq!(
            app.workspace.restore_non_terminal_focus(),
            WorkspaceFocus::Machines
        );
        app.set_focus(app.workspace.restore_non_terminal_focus());
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Machines);
    }

    #[test]
    fn inspector_keeps_the_last_image_as_its_terminal_resource() {
        let mut app = make_app();
        app.workspace.resources.catalog.images = vec![make_image("workstation")];
        app.workspace.resources.catalog.entries =
            vec![make_entry("workstation", MachineState::Running)];

        app.set_focus(WorkspaceFocus::Images);
        app.set_focus(WorkspaceFocus::ImageInspector);

        let image = app
            .focused_image_resource()
            .expect("Inspector should retain the image target");
        assert_eq!(image.name, "workstation");
        assert!(app.workspace.resources.image_has_running_machine(image));
    }

    #[test]
    fn image_terminal_requires_an_exact_running_machine() {
        let mut app = make_app();
        app.workspace.resources.catalog.images = vec![make_image("workstation")];
        app.workspace.resources.catalog.entries =
            vec![make_entry("workstation", MachineState::Starting)];
        app.set_focus(WorkspaceFocus::Images);

        let image = app.focused_image_resource().unwrap();
        assert!(!app.workspace.resources.image_has_running_machine(image));

        app.workspace.resources.catalog.entries[0].state = MachineState::Running;
        let image = app.focused_image_resource().unwrap();
        assert!(app.workspace.resources.image_has_running_machine(image));
    }

    #[tokio::test]
    async fn inspector_overview_uses_its_retained_image_target() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut app = make_app();
        app.workspace.resources.catalog.images =
            vec![make_image("retained"), make_image("list-selection")];
        app.workspace.resources.catalog.images[0].readonly = true;
        app.workspace.resources.catalog.image_selected = 1;
        app.workspace.inspector.state.target = DetailTarget::Image {
            name: "retained".into(),
            internal: false,
        };
        app.workspace
            .set_focus(WorkspaceFocus::ImageInspector, None);
        app.workspace
            .inspector
            .panel
            .ensure_pane_for_target(&app.workspace.inspector.state.target);

        app.refresh_detail_now().await;

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 18)).unwrap();
        let text = render_detail(&mut app, &mut terminal);
        let buffer = terminal.backend().buffer();
        assert!(text.contains("Name: retained"));
        assert!(!text.contains("list-selection"));
        let row_of = |expected: &str| {
            buffer
                .content
                .chunks(80)
                .position(|row| {
                    row[1..79]
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .trim_end()
                        == expected
                })
                .unwrap() as u16
        };
        assert_eq!(
            buffer[(1, row_of("Name: retained"))].fg,
            crate::tui::foundation::theme::theme().accent
        );
        assert_eq!(
            buffer[(12, row_of("Read only: yes"))].fg,
            crate::tui::foundation::theme::theme().prop_readonly_yes
        );

        let mut updated_images = app.workspace.resources.catalog.images.clone();
        updated_images[0].readonly = false;
        app.sync_snapshot(RuntimeSnapshot {
            machines: Vec::new(),
            images: updated_images,
        });
        assert!(app.workspace.inspector.state.overview_dirty);
        let text = render_detail(&mut app, &mut terminal);
        assert!(text.contains("Name: retained"));
        assert!(text.contains("Read only: no"));
    }

    #[tokio::test]
    async fn machine_overview_uses_its_retained_target_and_updates_without_inspection() {
        use crate::tui::features::workspace::inspector::DetailPane;
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("retained", MachineState::Running),
            make_entry("list-selection", MachineState::Running),
        ];
        app.workspace.resources.catalog.selected = 1;
        app.workspace.inspector.state.target = DetailTarget::Machine("retained".into());
        app.workspace
            .set_focus(WorkspaceFocus::MachineInspector, None);
        app.workspace.inspector.state.properties = Err("permission denied".into());

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let before = render_detail(&mut app, &mut terminal);
        assert_eq!(
            app.workspace.inspector.panel.active_pane(),
            DetailPane::MachineOverview
        );
        assert!(before.contains("Overview"));
        assert!(before.contains("Name: retained"));
        assert!(!before.contains("Name: list-selection"));
        assert!(before.contains("Status: running"));
        assert!(!app.workspace.inspector.state.overview_dirty);

        app.workspace.replace_machines(vec![
            make_entry("retained", MachineState::Exiting),
            make_entry("list-selection", MachineState::Running),
        ]);
        assert!(app.workspace.inspector.state.overview_dirty);
        let after = render_detail(&mut app, &mut terminal);
        assert!(after.contains("Status: exiting"));
        assert!(after.contains("permission denied"));
    }

    #[tokio::test]
    async fn detail_logs_and_metrics_follow_the_retained_target() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("retained", MachineState::Running),
            make_entry("list-selection", MachineState::Running),
        ];
        app.workspace.resources.catalog.selected = 1;
        app.workspace.inspector.state.target = DetailTarget::Machine("retained".into());
        app.workspace
            .resources
            .catalog
            .update_metrics("retained".into(), 1.0, 20.0, 256.0);
        for (name, line) in [
            ("retained", "retained log"),
            ("list-selection", "other log"),
        ] {
            app.workspace.inspector.logs.get_or_create(name);
            app.workspace.inspector.logs.push_line(name, line);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();

        app.workspace.inspector.panel.handle_key(
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT),
            &app.workspace.inspector.state.target,
        );
        let logs = render_detail(&mut app, &mut terminal);
        assert!(logs.contains("retained log"));
        assert!(!logs.contains("other log"));

        app.workspace.inspector.panel.handle_key(
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT),
            &app.workspace.inspector.state.target,
        );
        let metrics = render_detail(&mut app, &mut terminal);
        assert!(metrics.contains("CPU Usage (%)"));
        assert!(metrics.contains("Memory Usage"));
        assert!(!metrics.contains("Waiting for metrics data"));
    }

    #[tokio::test]
    async fn detail_log_layout_changes_with_the_target_and_releases_stale_offsets() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("first", MachineState::Running),
            make_entry("second", MachineState::Running),
        ];
        app.workspace.inspector.state.target = DetailTarget::Machine("first".into());
        for (name, line) in [
            ("first", "first log".into()),
            ("second", "second ".repeat(30)),
        ] {
            let buffer = app.workspace.inspector.logs.get_or_create(name);
            buffer.lines.push_back(line);
            // Neither buffer has new output when the visible target changes.
            buffer.dirty = false;
        }
        app.workspace.inspector.panel.handle_key(
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT),
            &app.workspace.inspector.state.target,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        assert!(render_detail(&mut app, &mut terminal).contains("first log"));
        assert_eq!(app.workspace.inspector.panel.logs_len, 1);

        app.workspace
            .inspector
            .state
            .replace_target(DetailTarget::Machine("second".into()));
        assert!(render_detail(&mut app, &mut terminal).contains("second"));
        assert!(app.workspace.inspector.panel.logs_len > 1);

        app.workspace.replace_machines(Vec::new());
        assert!(!render_detail(&mut app, &mut terminal).contains("second"));
        assert_eq!(app.workspace.inspector.panel.logs_len, 0);
        assert!(app
            .workspace
            .inspector
            .panel
            .log_cache
            .offset_index
            .is_empty());
    }

    #[tokio::test]
    async fn image_details_read_unit_data_and_clear_stale_state_for_non_machine_image_name() {
        let name = "Ubuntu Resolute 镜像";
        let mut app = make_app();
        app.workspace.resources.catalog.images = vec![make_image(name)];
        app.workspace.inspector.state.target = DetailTarget::Image {
            name: name.into(),
            internal: false,
        };
        app.workspace.inspector.state.unit_name = Some("systemd-nspawn@stale.service".into());
        app.workspace.inspector.state.unit_drop_ins.push(
            crate::application::inspection::SystemdDropInInspection {
                path: "/etc/systemd/system/systemd-nspawn@stale.service.d/stale.conf".into(),
                content: "[Service]\nEnvironment=STALE=yes".into(),
            },
        );
        app.workspace
            .set_focus(WorkspaceFocus::ImageInspector, None);
        app.workspace
            .inspector
            .panel
            .ensure_pane_for_target(&app.workspace.inspector.state.target);
        app.workspace.inspector.panel.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('3'),
                crossterm::event::KeyModifiers::ALT,
            ),
            &app.workspace.inspector.state.target,
        );

        assert_eq!(
            app.workspace.inspector.panel.active_pane(),
            crate::tui::features::workspace::inspector::DetailPane::Details,
        );

        app.refresh_detail_now().await;

        assert!(app.workspace.inspector.state.unit_name.is_none());
        assert!(app.workspace.inspector.state.unit_drop_ins.is_empty());
        assert!(app
            .workspace
            .inspector
            .state
            .properties
            .as_ref()
            .unwrap()
            .groups
            .is_empty());
    }

    #[test]
    fn completed_detail_read_cannot_overwrite_a_new_selection() {
        use crate::tui::features::workspace::inspector::refresh::{
            DetailRefreshCompletion, DetailRefreshResult,
        };

        let mut app = make_app();
        app.workspace.resources.catalog.entries = vec![
            make_entry("first", MachineState::Running),
            make_entry("second", MachineState::Running),
        ];
        app.set_focus(WorkspaceFocus::Machines);
        app.request_detail_refresh();
        let stale_ticket = app
            .workspace
            .inspector
            .state
            .refresh
            .take_pending()
            .unwrap();

        app.workspace.resources.catalog.selected = 1;
        app.request_detail_refresh();
        assert_eq!(
            app.workspace.inspector.state.target,
            DetailTarget::Machine("second".into())
        );

        let mut stale_properties = MachineProperties::default();
        stale_properties.insert("Machine", "Name".into(), "first".into());
        app.apply_detail_refresh(DetailRefreshCompletion {
            ticket: stale_ticket,
            result: DetailRefreshResult::MachineProperties(Ok(crate::application::RuntimeQuery {
                value: stale_properties,
                route: crate::application::operations::ExecutionRoute::DirectDbus,
                fallback: None,
            })),
        });

        assert!(app
            .workspace
            .inspector
            .state
            .properties
            .as_ref()
            .unwrap()
            .groups
            .is_empty());
        assert!(app.workspace.inspector.state.refresh.has_pending());
    }

    #[tokio::test]
    async fn pointer_selection_targets_the_hit_list_independently_of_keyboard_focus() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut app = app_with_machine_and_image();
        app.workspace
            .resources
            .catalog
            .entries
            .push(make_entry("second-machine", MachineState::Running));
        app.workspace
            .resources
            .catalog
            .images
            .extend([make_image("second-image"), make_image("third-image")]);
        let mut display =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        display
            .draw(|frame| {
                crate::tui::app::draw(frame, &mut app);
            })
            .unwrap();
        let machines = app.workspace.panel_areas().machines;
        let images = app.workspace.panel_areas().images;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: images.x + 2,
            row: images.y + 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.workspace.resources.catalog.image_selected, 1);
        assert_eq!(app.workspace.resources.catalog.selected, 0);
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Images);
        assert_eq!(
            app.workspace.inspector.state.target,
            DetailTarget::Image {
                name: "second-image".into(),
                internal: false,
            }
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: images.x + 2,
            row: images.y + 3,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.workspace.resources.catalog.image_selected, 2);
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Images);
        assert_eq!(
            app.workspace.inspector.state.target,
            DetailTarget::Image {
                name: "third-image".into(),
                internal: false,
            }
        );

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.workspace.resources.catalog.image_selected, 1);
        assert_eq!(app.workspace.resources.catalog.selected, 0);

        // Repaint at a smaller size. Mouse dispatch must use the new
        // panel and row bounds rather than geometry from the first frame.
        let mut smaller =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        smaller
            .draw(|frame| {
                crate::tui::app::draw(frame, &mut app);
            })
            .unwrap();
        let resized_machines = app.workspace.panel_areas().machines;
        assert!(resized_machines.width < machines.width);
        assert!(resized_machines.height < machines.height);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: resized_machines.x + 2,
            row: resized_machines.y + 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.workspace.resources.catalog.selected, 1);
        assert_eq!(app.workspace.resources.catalog.image_selected, 1);
        assert_eq!(app.workspace.focus(), WorkspaceFocus::Machines);
        assert_eq!(
            app.workspace.inspector.state.target,
            DetailTarget::Machine("second-machine".into())
        );
    }

    #[tokio::test]
    async fn visible_help_consumes_mouse_before_background_focus() {
        let mut app = make_app();
        app.workspace
            .set_focus(WorkspaceFocus::MachineInspector, None);
        app.ui.overlays.open_help();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                crate::tui::app::draw(frame, &mut app);
            })
            .unwrap();

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;

        assert_eq!(app.workspace.focus(), WorkspaceFocus::MachineInspector);
    }
}

#[test]
fn image_refresh_sorts_by_name_and_preserves_selection() {
    let mut app = make_app();
    app.workspace.resources.catalog.images = vec![make_image("selected")];
    app.workspace.resources.catalog.internal_images =
        vec![make_internal_image(".selected-internal")];

    app.sync_snapshot(RuntimeSnapshot::new(
        vec![],
        vec![
            make_image("z-image"),
            make_image("selected"),
            make_image("a-image"),
            make_internal_image(".z-internal"),
            make_internal_image(".selected-internal"),
            make_internal_image(".a-internal"),
        ],
    ));

    let names: Vec<_> = app
        .workspace
        .resources
        .catalog
        .images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    assert_eq!(names, ["a-image", "selected", "z-image"]);
    assert_eq!(app.workspace.resources.catalog.image_selected, 1);
    let internal_names: Vec<_> = app
        .workspace
        .resources
        .catalog
        .internal_images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    assert_eq!(
        internal_names,
        [".a-internal", ".selected-internal", ".z-internal"]
    );
    assert_eq!(app.workspace.resources.catalog.internal_image_selected, 1);
}

#[test]
fn machine_refresh_keeps_every_present_runtime_state() {
    let mut app = make_app();

    app.sync_snapshot(RuntimeSnapshot::new(
        vec![
            make_entry("running", MachineState::Running),
            make_entry("starting", MachineState::Starting),
            make_entry("exiting", MachineState::Exiting),
        ],
        vec![],
    ));

    assert_eq!(
        app.workspace
            .resources
            .catalog
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.state.clone()))
            .collect::<Vec<_>>(),
        [
            ("exiting", MachineState::Exiting),
            ("running", MachineState::Running),
            ("starting", MachineState::Starting),
        ]
    );
}

mod machine_actions {
    use super::*;
    use crate::application::machine_lifecycle::{
        MachineControlOutcome, MockMachineControl, MockMachineObservation,
        MockMachineStartDiagnostics, MockMachineStartPreparation, RoutedMachineControlOutcome,
    };
    use crate::application::operations::route::ExecutionRoute;
    use crate::application::OperationRegistry;

    fn prepare_poweroff_app(
        control_outcome: MachineControlOutcome,
        requests: usize,
    ) -> (App, tokio::sync::mpsc::Receiver<AppEvent>) {
        let mut control = MockMachineControl::new();
        control
            .expect_execute_runtime()
            .times(requests)
            .returning(move |_, action| {
                assert_eq!(action, crate::application::MachineRuntimeAction::Poweroff);
                RoutedMachineControlOutcome {
                    outcome: control_outcome.clone(),
                    route: ExecutionRoute::DirectDbus,
                    fallback: None,
                }
            });
        let preparation = MockMachineStartPreparation::new();
        let mut observation = MockMachineObservation::new();
        observation
            .expect_invalidate()
            .times(requests)
            .return_const(());
        let diagnostics = MockMachineStartDiagnostics::new();
        let lifecycle = std::sync::Arc::new(MachineLifecycleService::new(
            std::sync::Arc::new(control),
            std::sync::Arc::new(preparation),
            std::sync::Arc::new(observation),
            std::sync::Arc::new(diagnostics),
            OperationRegistry::new(),
        ));
        let mut app = make_app();
        app.services.machine_lifecycle = lifecycle;
        app.sync_snapshot(RuntimeSnapshot::new(
            vec![
                make_entry("first", MachineState::Running),
                make_entry("second", MachineState::Running),
            ],
            vec![],
        ));
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        app.ui.app_tx = Some(tx);
        (app, rx)
    }

    #[tokio::test]
    async fn different_machines_accept_back_to_back_poweroff_actions() {
        let (mut app, mut rx) = prepare_poweroff_app(MachineControlOutcome::Succeeded, 2);

        app.workspace.resources.catalog.selected = 0;
        app.action_poweroff();
        app.workspace.resources.catalog.selected = 1;
        app.action_poweroff();

        assert!(app
            .workspace
            .resources
            .catalog
            .entries
            .iter()
            .all(|entry| entry.state == MachineState::Exiting));

        let mut completed = Vec::new();
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("both poweroff actions should finish")
                .expect("both poweroff actions should report a result");
            let AppEvent::MachineOperationFinished(outcome) = event else {
                panic!("poweroff should report a machine lifecycle outcome");
            };
            let name = outcome.machine.as_str().to_string();
            let (message, level) =
                crate::tui::features::workspace::resources::actions::machine_outcome_status(
                    outcome,
                );
            assert_eq!(message, format!("Poweroff requested for {name}"));
            assert_eq!(level, crate::tui::foundation::status::StatusLevel::Success);
            completed.push(name);
        }
        completed.sort();

        assert_eq!(completed, ["first", "second"]);
    }

    #[tokio::test]
    async fn failed_poweroff_restores_the_observed_state_without_another_query() {
        let (mut app, mut rx) = prepare_poweroff_app(
            MachineControlOutcome::Failed {
                reason: "poweroff rejected".into(),
            },
            1,
        );
        app.action_poweroff();
        assert_eq!(
            app.workspace.resources.machine("first").unwrap().state,
            MachineState::Exiting
        );

        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("poweroff task should finish")
            .expect("poweroff task should report a result");
        let AppEvent::MachineOperationFinished(outcome) = event else {
            panic!("poweroff should report a machine operation outcome");
        };
        app.handle_machine_operation_result(outcome);

        assert_eq!(
            app.workspace.resources.catalog.entries,
            vec![
                make_entry("first", MachineState::Running),
                make_entry("second", MachineState::Running),
            ]
        );
        assert!(app
            .ui
            .status_message
            .as_ref()
            .unwrap()
            .0
            .contains("poweroff rejected"));
    }
}

mod set_status {
    use super::*;

    #[test]
    fn sets_message_and_expiry() {
        let mut app = make_app();
        app.set_status(
            "hello".into(),
            crate::tui::foundation::status::StatusLevel::Info,
        );

        let (msg, level) = app.ui.status_message.as_ref().unwrap();
        assert_eq!(msg, "hello");
        assert_eq!(*level, crate::tui::foundation::status::StatusLevel::Info);
        assert!(app.ui.status_expiry.is_some());
    }
}
