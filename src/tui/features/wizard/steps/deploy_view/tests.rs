use super::*;
use crate::application::provisioning::{deployment_job_channel, DeploymentId};
use crate::domain::machine::accounts::LocalAccountObservation;

#[test]
fn background_updates_advance_the_view_and_close_obsolete_dialogs_before_rendering() {
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
    let (handle, context) = deployment_job_channel(DeploymentId::from_u128(4));
    let mut view = DeployStepView::new(handle);
    view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(view.cancel_dialog.is_some());

    context
        .event_sender()
        .try_send(DeploymentEvent::Line("last output".into()))
        .unwrap();
    context
        .status_sender()
        .send_replace(DeploymentStatus::Failed("deployment failed".into()));

    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    for _ in 0..2 {
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
    }
    assert!(view.cancel_dialog.is_some());
    assert_eq!(view.logs.len(), 0);

    view.poll_updates();
    assert!(view.cancel_dialog.is_none());
    assert_eq!(view.logs.len(), 1);
    assert_eq!(view.status, "FAILED: deployment failed");
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        EventResult::Message(AppMessage::Wizard(WizardMessage::Close))
    );
    assert!(!context.cancellation().is_requested());
}

#[test]
fn deployment_logs_reflow_and_keep_browsing_position_until_end_is_requested() {
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
    let (handle, context) = deployment_job_channel(DeploymentId::from_u128(3));
    let events = context.event_sender();
    for index in 0..30 {
        events
            .try_send(DeploymentEvent::Line(format!(
                "entry-{index:02} {} tail-{index:02}",
                "words 中文 👩‍💻 ".repeat(8)
            )))
            .unwrap();
    }
    let mut view = DeployStepView::new(handle);
    view.poll_updates();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    let draw = |view: &mut DeployStepView, terminal: &mut Terminal<TestBackend>| {
        let rendered = terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        rendered
            .buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    assert!(draw(&mut view, &mut terminal).contains("tail-29"));
    view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert!(draw(&mut view, &mut terminal).contains("entry-00"));

    events
        .try_send(DeploymentEvent::Line("newest-entry".into()))
        .unwrap();
    view.poll_updates();
    assert!(!draw(&mut view, &mut terminal).contains("newest-entry"));
    terminal.backend_mut().resize(24, 24);
    assert!(draw(&mut view, &mut terminal).contains("entry-00"));

    view.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert!(!draw(&mut view, &mut terminal).contains("entry-00"));
    view.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert!(draw(&mut view, &mut terminal).contains("newest-entry"));
    terminal.backend_mut().resize(80, 24);
    assert!(draw(&mut view, &mut terminal).contains("newest-entry"));
    assert_eq!(view.logs.len(), 31);
}

#[test]
fn completed_account_snapshot_can_be_scrolled_to_the_last_account() {
    use crate::application::provisioning::DeploymentResult;
    use crate::domain::machine::accounts::LocalAccount;
    use ratatui::{backend::TestBackend, Terminal};

    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());

    let (handle, context) = deployment_job_channel(DeploymentId::from_u128(2));
    context
        .status_sender()
        .send_replace(DeploymentStatus::Succeeded(DeploymentResult {
            local_accounts: Some(LocalAccountObservation::Read {
                accounts: (0..40)
                    .map(|index| LocalAccount {
                        username: format!("existing-{index}"),
                        uid: 501 + index,
                        gid: 20,
                        home: format!("/home/existing-{index}"),
                        shell: "/bin/sh".into(),
                    })
                    .collect(),
                rejected_lines: vec![],
            }),
        }));
    let mut view = DeployStepView::new(handle);
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    view.form.select(DeployControl::Accounts);
    let _ = view.handle_key(KeyEvent::new(
        KeyCode::End,
        crossterm::event::KeyModifiers::NONE,
    ));
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("/home/existing-39"));
    assert!(text.contains("/bin/sh"));
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
