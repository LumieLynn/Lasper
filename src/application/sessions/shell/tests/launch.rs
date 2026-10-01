use super::*;

#[tokio::test]
async fn x11_is_not_prepared_without_an_explicit_selection() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let x11 = RecordingX11Preparation::default();
    let mut opened = sessions
        .launch_shell(launch_request(WaylandShellRequest::Disabled), Some(&x11))
        .await
        .unwrap();

    assert!(opened.x11.is_none());
    assert!(x11.calls.lock().is_empty());
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    opened.handle.close();
}

#[tokio::test]
async fn current_and_exact_x11_selections_reach_the_terminal() {
    for (selection, display) in [
        (X11SessionSelection::Current, 0),
        (X11SessionSelection::Display(1), 1),
    ] {
        let port = Arc::new(FallbackSessionPort::new(false));
        let sessions = SessionService::new(port.clone());
        let x11 = RecordingX11Preparation::default();
        let mut opened = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Disabled).with_x11(selection),
                Some(&x11),
            )
            .await
            .unwrap();

        let prepared = opened.x11.as_ref().unwrap();
        assert_eq!(prepared.context().display(), display);
        let requests = port.requests.lock();
        let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
            panic!("expected selected-user shell");
        };
        assert_eq!(environment.x11_context(), Some(prepared.context()));
        assert_eq!(x11.calls.lock().len(), 1);
        opened.handle.close();
    }
}

#[tokio::test]
async fn explicit_x11_failure_never_opens_a_fallback_terminal() {
    for fail_projection in [true, false] {
        let port = Arc::new(FallbackSessionPort::new(false));
        let sessions = SessionService::new(port.clone());
        let x11 = RecordingX11Preparation {
            fail_projection,
            fail_authorization: !fail_projection,
            ..Default::default()
        };
        let result = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Automatic)
                    .with_x11(X11SessionSelection::Current)
                    .with_wayland_fallback(true),
                Some(&x11),
            )
            .await;

        assert!(matches!(result, Err(ShellLaunchError::X11(_))));
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 0);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
        assert_eq!(x11.calls.lock().len(), 1);
    }
}

#[tokio::test]
async fn explicit_x11_without_an_access_service_is_an_error() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let result = sessions
        .launch_shell(
            launch_request(WaylandShellRequest::Disabled).with_x11(X11SessionSelection::Current),
            None,
        )
        .await;

    assert!(matches!(
        result,
        Err(ShellLaunchError::X11(X11AccessError::Selection(_)))
    ));
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn wayland_fallback_preserves_x11_command_environment_and_size() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let sessions = SessionService::new(port.clone());
    let x11 = RecordingX11Preparation::default();
    let target = shell_intent(WaylandShellRequest::Automatic)
        .target()
        .clone();
    let terminal_environment = InteractiveShellEnvironment::new(
        "xterm-256color".into(),
        Some("truecolor".into()),
        Some(String::new()),
    )
    .unwrap();
    let command =
        GuestCommand::new("/usr/bin/kitty", vec!["--class".into(), "demo".into()]).unwrap();
    let size = SessionSize::new(111, 37).unwrap();
    let request = ShellLaunchRequest::new(
        target.clone(),
        WaylandShellRequest::Automatic,
        terminal_environment.clone(),
        size,
    )
    .with_command(command.clone())
    .with_x11(X11SessionSelection::Display(1))
    .with_wayland_fallback(true);
    let mut opened = sessions.launch_shell(request, Some(&x11)).await.unwrap();

    assert!(matches!(
        opened.wayland_fallback,
        Some(WaylandFallbackCause::Validation(_))
    ));
    assert_eq!(x11.calls.lock().len(), 1);
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    let requests = port.requests.lock();
    assert_eq!(requests.len(), 1);
    let terminal = &requests[0];
    assert_eq!(&terminal.machine, target.machine());
    assert_eq!(terminal.size, size);
    let TerminalLaunch::SelectedUserShell {
        user,
        environment,
        command: launched_command,
    } = &terminal.launch
    else {
        panic!("expected selected-user shell");
    };
    assert_eq!(user, target.user());
    assert_eq!(launched_command.as_ref(), Some(&command));
    assert_eq!(environment.terminal_environment(), &terminal_environment);
    assert!(environment.wayland_context().is_none());
    assert_eq!(
        environment.x11_context(),
        Some(opened.x11.as_ref().unwrap().context())
    );
    opened.handle.close();
}

#[tokio::test]
async fn wayland_and_x11_can_be_prepared_together() {
    let port = Arc::new(FallbackSessionPort {
        probe_succeeds: true,
        ..FallbackSessionPort::new(false)
    });
    let sessions = SessionService::new(port.clone());
    let x11 = RecordingX11Preparation::default();
    let mut opened = sessions
        .launch_shell(
            launch_request(WaylandShellRequest::Automatic)
                .with_x11(X11SessionSelection::Current)
                .with_wayland_fallback(true),
            Some(&x11),
        )
        .await
        .unwrap();

    assert!(opened.wayland_fallback.is_none());
    assert_eq!(x11.calls.lock().len(), 1);
    let requests = port.requests.lock();
    let TerminalLaunch::SelectedUserShell { environment, .. } = &requests[0].launch else {
        panic!("expected selected-user shell");
    };
    assert!(environment.wayland_context().is_some());
    assert!(environment.x11_context().is_some());
    opened.handle.close();
}

#[tokio::test]
async fn terminal_failure_retains_the_x11_preparation_without_reauthorizing() {
    for probe_succeeds in [true, false] {
        let port = Arc::new(FallbackSessionPort {
            probe_succeeds,
            ..FallbackSessionPort::new(true)
        });
        let sessions = SessionService::new(port.clone());
        let x11 = RecordingX11Preparation::default();
        let result = sessions
            .launch_shell(
                launch_request(WaylandShellRequest::Automatic)
                    .with_x11(X11SessionSelection::Current)
                    .with_wayland_fallback(true),
                Some(&x11),
            )
            .await;

        let Err(ShellLaunchError::Open {
            error,
            x11: Some(prepared),
        }) = result
        else {
            panic!("expected terminal failure with prepared X11 access");
        };
        assert_eq!(prepared.context().display(), 0);
        assert!(matches!(
            prepared.disposition(),
            X11AuthorizationDisposition::Added { .. }
        ));
        assert_eq!(
            matches!(error, ShellAttemptError::Initial(_)),
            probe_succeeds
        );
        assert_eq!(x11.calls.lock().len(), 1);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    }
}
