use super::*;

#[tokio::test]
async fn interactive_wayland_probe_retries_once_without_wayland() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    let (mut handle, used_fallback) = service
        .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
        .await
        .unwrap();

    assert!(matches!(
        used_fallback,
        Some(WaylandFallbackCause::Validation(_))
    ));
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    assert_eq!(*port.open_wayland.lock(), [false]);
    handle.close();
}

#[tokio::test]
async fn explicit_wayland_failure_does_not_retry() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    let error = match service
        .open_shell_with_fallback(
            shell_intent(WaylandShellRequest::Display(
                WaylandDisplay::new("wayland-0").unwrap(),
            )),
            true,
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("explicit Wayland failure unexpectedly opened a shell"),
    };

    assert!(matches!(
        error,
        ShellAttemptError::Initial(ShellOpenError::WaylandPreparation(_))
    ));
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn wayland_fallback_failure_is_reported_as_fallback_error() {
    let port = Arc::new(FallbackSessionPort::new(true));
    let service = SessionService::new(port.clone());
    let error = match service
        .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("Wayland fallback unexpectedly succeeded"),
    };

    assert!(matches!(
        error,
        ShellAttemptError::Fallback {
            cause: WaylandFallbackCause::Validation(_),
            error: ShellOpenError::Terminal(_),
        }
    ));
    assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 1);
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn exact_display_selection_uses_all_discovered_sockets() {
    let selected = select_wayland_socket(
        vec![socket("wayland-0", 1), socket("wayland-1", 2)],
        &WaylandDisplay::new("wayland-1").unwrap(),
    )
    .unwrap();

    assert_eq!(selected.display().as_str(), "wayland-1");
    assert!(select_wayland_socket(
        vec![socket("wayland-0", 1)],
        &WaylandDisplay::new("wayland-2").unwrap(),
    )
    .unwrap_err()
    .contains("available: wayland-0"));
}

#[tokio::test]
async fn automatic_launcher_does_not_fall_back() {
    let port = Arc::new(FallbackSessionPort::new(false));
    let service = SessionService::new(port.clone());
    assert!(matches!(
        service
            .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), false)
            .await,
        Err(ShellAttemptError::Initial(
            ShellOpenError::WaylandPreparation(_)
        ))
    ));
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn terminal_failure_is_never_retried() {
    let port = Arc::new(FallbackSessionPort {
        probe_succeeds: true,
        ..FallbackSessionPort::new(true)
    });
    let service = SessionService::new(port.clone());
    assert!(matches!(
        service
            .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
            .await,
        Err(ShellAttemptError::Initial(ShellOpenError::Terminal(_)))
    ));
    assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    assert_eq!(*port.open_wayland.lock(), [true]);
}

#[tokio::test]
async fn selection_failure_preserves_its_cause_through_fallback() {
    for fail_fallback in [false, true] {
        let port = Arc::new(FallbackSessionPort {
            fail_selection: true,
            ..FallbackSessionPort::new(fail_fallback)
        });
        let service = SessionService::new(port.clone());
        let result = service
            .open_shell_with_fallback(shell_intent(WaylandShellRequest::Automatic), true)
            .await;
        if fail_fallback {
            assert!(matches!(
                result,
                Err(ShellAttemptError::Fallback {
                    cause: WaylandFallbackCause::SocketSelection(_),
                    ..
                })
            ));
        } else {
            assert!(matches!(
                result,
                Ok((_, Some(WaylandFallbackCause::SocketSelection(_))))
            ));
        }
        assert_eq!(port.prepare_calls.load(Ordering::Relaxed), 0);
        assert_eq!(port.open_calls.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn disabled_wayland_skips_discovery_and_probe() {
    let port = Arc::new(CountingSessionPort::default());
    let sessions = SessionService::new(port.clone());
    let target = ShellTarget::new(
        MachineName::new("demo").unwrap(),
        ValidatedGuestUserName::new("alice").unwrap(),
    );
    let request = sessions
        .resolve_shell_wayland(&target, &WaylandShellRequest::Disabled)
        .await
        .unwrap();

    assert!(request.is_none());
    assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 0);
    assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn automatic_wayland_uses_machine_aware_selection_only() {
    let port = Arc::new(CountingSessionPort::default());
    let sessions = SessionService::new(port.clone());
    let target = ShellTarget::new(
        MachineName::new("demo").unwrap(),
        ValidatedGuestUserName::new("alice").unwrap(),
    );

    let request = sessions
        .resolve_shell_wayland(&target, &WaylandShellRequest::Automatic)
        .await
        .unwrap();

    assert!(request.is_none());
    assert_eq!(port.automatic_calls.load(Ordering::Relaxed), 1);
    assert_eq!(port.discovery_calls.load(Ordering::Relaxed), 0);
}
