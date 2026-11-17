use super::fixture::{command, exit_report, final_exit, ready, Server};
use lasper_guest_host_transport::{SessionPhase, SessionSnapshot};
use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitReason, ExitReport, ExitStatus, PreparationControlKind,
};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn runtime_shutdown_publishes_driver_stopped_without_confirming_cleanup() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        assert!(crate::control::receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let (server, prepared) = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let prepared = runtime.block_on(server.prepare_client());
        drop(runtime);
        (server, prepared)
    })
    .join()
    .unwrap();
    let state = prepared.session.subscribe();
    assert_eq!(state.borrow().phase, SessionPhase::Finished);
    assert!(matches!(
        state.borrow().error(),
        Some(lasper_guest_host_transport::BrokerSessionError::DriverStopped)
    ));
    let completion = prepared.session.wait().await;
    assert_eq!(
        completion.submission,
        lasper_guest_host_transport::StartSubmission::NotSubmitted
    );
    assert!(completion.report.is_none());
    assert!(matches!(
        completion.error.as_deref(),
        Some(lasper_guest_host_transport::BrokerSessionError::DriverStopped)
    ));
    drop(prepared);
    server.finish().await;
}

#[tokio::test]
async fn public_concurrent_close_sends_one_cancel_and_shares_completion() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        super::fixture::reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
        assert!(crate::control::receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let prepared = server.prepare_client().await;
    let (first, second) = tokio::join!(prepared.session.close(), prepared.session.close());
    assert!(first.error.is_none());
    assert_eq!(first.report, second.report);
    assert_eq!(
        first.submission,
        lasper_guest_host_transport::StartSubmission::NotSubmitted
    );
    assert!(first.report.unwrap().is_final());
    drop(prepared);
    server.finish().await;
}

#[tokio::test]
async fn cancelling_public_close_does_not_resubmit_cleanup() {
    let (closing, closed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        closing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        super::fixture::reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
        assert!(crate::control::receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let prepared = server.prepare_client().await;
    let mut close = Box::pin(prepared.session.close());
    crate::session_clock::drive_io(async {
        tokio::select! {
            result = &mut close => panic!("cleanup not acknowledged: {result:?}"),
            result = closed => result.unwrap(),
        }
    })
    .await;
    drop(close);
    release.send(()).unwrap();
    let completion = crate::session_clock::drive_io(prepared.session.close()).await;
    assert!(completion.error.is_none());
    assert!(completion.report.unwrap().is_final());
    drop(prepared);
    server.finish().await;
}

#[tokio::test]
async fn cleanup_winning_a_lease_request_preserves_the_final_exit() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 1000);
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::KeepPrepared
        );
        final_exit(&guest, &request, ExitReason::LeaseExpired);
    });
    let mut session = server.prepare().await;
    timeout(Duration::from_secs(2), async {
        while session.snapshot().phase() != SessionPhase::Finished {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let state = session.snapshot();
    assert!(state.error().is_none());
    assert_eq!(state.report.unwrap().reason(), ExitReason::LeaseExpired);
    server.finish().await;
}

#[tokio::test]
async fn cleanup_winning_drop_is_still_a_successful_final_report() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::Cancel
        );
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    drop(session);
    let state = wait_for(&mut state, SessionPhase::Finished).await;
    assert!(state.error().is_none());
    assert_eq!(
        state.report.unwrap().reason(),
        ExitReason::WorkerExitedBeforeStart
    );
    server.finish().await;
}

#[tokio::test]
async fn cleanup_progress_replaces_reports_and_closing_rejects_new_commands() {
    let (advance, next) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        next.recv_timeout(Duration::from_secs(3)).unwrap();
        for cleanup in [
            CleanupStatus::InProgress,
            CleanupStatus::Pending { errno: None },
        ] {
            let report = ExitReport::new(
                ExecutionStatus::NotStarted,
                None,
                cleanup,
                ExitReason::ClientClosed,
            )
            .unwrap();
            exit_report(&guest, &request, report);
            next.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    advance.send(()).unwrap();
    for expected in [
        CleanupStatus::InProgress,
        CleanupStatus::Pending { errno: None },
    ] {
        let snapshot = timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = state.borrow_and_update().clone();
                if snapshot
                    .report
                    .is_some_and(|exit| exit.cleanup() == expected)
                {
                    break snapshot;
                }
                state.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshot.phase(), SessionPhase::Closing);
        assert!(snapshot.report.is_some());
        assert!(matches!(
            session.recheck().await,
            Err(
                lasper_guest_host_transport::BrokerSessionError::InvalidState(
                    SessionPhase::Closing
                )
            )
        ));
        advance.send(()).unwrap();
    }
    let snapshot = wait_for(&mut state, SessionPhase::Finished).await;
    assert!(snapshot.error().is_none());
    let exit = snapshot.report.unwrap();
    assert_eq!(exit.exit(), Some(ExitStatus::Signaled(15)));
    server.finish().await;
}

#[tokio::test]
async fn eof_during_cleanup_retains_the_last_report_without_claiming_completion() {
    let (release, finish) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        finish.recv_timeout(Duration::from_secs(3)).unwrap();
        let report = ExitReport::new(
            ExecutionStatus::NotStarted,
            None,
            CleanupStatus::InProgress,
            ExitReason::ClientClosed,
        )
        .unwrap();
        exit_report(&guest, &request, report);
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    release.send(()).unwrap();
    let snapshot = wait_for(&mut state, SessionPhase::Finished).await;
    assert!(snapshot.error().is_some());
    let exit = snapshot.report.unwrap();
    assert_eq!(exit.cleanup(), CleanupStatus::InProgress);
    assert!(!exit.is_final());
    server.finish().await;
}

async fn wait_for(
    state: &mut tokio::sync::watch::Receiver<SessionSnapshot>,
    phase: SessionPhase,
) -> SessionSnapshot {
    timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if snapshot.phase() == phase {
                break snapshot;
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}
