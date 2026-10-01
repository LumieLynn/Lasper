use super::*;
use crate::application::sessions::X11SessionContext;

#[tokio::test]
async fn selected_socket_launch_prepares_before_opening_with_verified_context() {
    let port = Arc::new(FallbackSessionPort {
        probe_succeeds: true,
        ..FallbackSessionPort::new(false)
    });
    let service = SessionService::new(port.clone());
    let selected = socket("wayland-0", 2);
    let request = launch_request(WaylandShellRequest::SelectedHostDisplay(selected.clone()));
    let mut opened = service.launch_shell(request, None).await.unwrap();

    assert!(opened.x11.is_none());
    assert!(opened.wayland_fallback.is_none());
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    let requests = port.requests.lock();
    assert_eq!(requests.len(), 1);
    let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
        panic!("expected selected-user shell");
    };
    assert_eq!(
        environment.wayland_context().unwrap().host_socket(),
        &selected
    );
    assert_eq!(environment.terminal_environment().term(), "dumb");
    opened.handle.close();
}

#[tokio::test]
async fn preselected_socket_failure_never_uses_fallback() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    let request = launch_request(WaylandShellRequest::SelectedHostDisplay(socket(
        "wayland-0",
        2,
    )))
    .with_wayland_fallback(true);
    let result = service.launch_shell(request, None).await;

    assert!(matches!(
        result,
        Err(ShellLaunchError::Open {
            error: ShellAttemptError::Initial(ShellOpenError::WaylandPreparation(_)),
            x11: None,
        })
    ));
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn guest_command_is_carried_without_display_integration() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    let command = GuestCommand::new("/usr/bin/kitty", vec!["--single-instance".into()]).unwrap();
    let request = launch_request(WaylandShellRequest::Disabled).with_command(command.clone());
    let mut opened = service.launch_shell(request, None).await.unwrap();

    assert!(opened.x11.is_none());
    assert!(opened.wayland_fallback.is_none());
    let requests = port.requests.lock();
    let TerminalLaunch::SelectedUserShell {
        environment,
        command: actual,
        ..
    } = &requests[0].launch
    else {
        panic!("expected selected-user shell");
    };
    assert_eq!(actual.as_ref(), Some(&command));
    assert!(environment.wayland_context().is_none());
    assert!(environment.x11_context().is_none());
    opened.handle.close();
}

#[tokio::test]
async fn prepared_x11_context_is_target_bound_and_carried_without_reprobing() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    let intent = shell_intent(WaylandShellRequest::Disabled);
    let context = X11SessionContext::prepared(intent.target().clone(), x11_projection(0));
    let intent = intent.with_x11(context.clone());
    let mut terminal = service.open_prepared_shell(&intent, None).await.unwrap();
    {
        let requests = port.requests.lock();
        let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
            panic!("expected selected-user shell");
        };
        assert_eq!(environment.x11_context(), Some(&context));
    }

    let mismatched = ShellOpenIntent::new(
        ShellTarget::new(
            MachineName::new("other").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        ),
        WaylandShellRequest::Disabled,
        InteractiveShellEnvironment::default(),
        SessionSize::new(80, 24).unwrap(),
    )
    .with_x11(context);
    assert!(matches!(
        service.open_prepared_shell(&mismatched, None).await,
        Err(ShellOpenError::X11Context(_))
    ));
    assert_eq!(port.requests.lock().len(), 1);
    terminal.close();
}
