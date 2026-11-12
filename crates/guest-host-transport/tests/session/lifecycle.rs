use super::fixture::{command, exit_report, final_exit, ready, Server};
use lasper_guest_host_transport::{SessionPhase, SessionSnapshot};
use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitReason, ExitReport, ExitStatus, OutputStream,
    PreparationControlKind,
};
use std::time::Duration;
use tokio::time::timeout;

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
        while session.snapshot().phase() != SessionPhase::Closed {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let state = session.snapshot();
    assert!(state.error().is_none());
    assert_eq!(
        state.exit().unwrap().report().reason(),
        ExitReason::LeaseExpired
    );
    server.finish().await;
}

#[tokio::test]
async fn cleanup_winning_close_is_still_a_successful_final_report() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::Close
        );
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    drop(session);
    let state = wait_for(&mut state, SessionPhase::Closed).await;
    assert!(state.error().is_none());
    assert_eq!(
        state.exit().unwrap().report().reason(),
        ExitReason::WorkerExitedBeforeStart
    );
    server.finish().await;
}

#[tokio::test]
async fn cleanup_progress_replaces_output_and_closing_rejects_new_commands() {
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
            exit_report(
                &guest,
                &request,
                report,
                OutputStream::new(b"partial", false, false).unwrap(),
            );
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
                    .exit()
                    .is_some_and(|exit| exit.report().cleanup() == expected)
                {
                    break snapshot;
                }
                state.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshot.phase(), SessionPhase::Closing);
        let exit = snapshot.exit().unwrap();
        assert_eq!(exit.output().unwrap().stdout().bytes(), b"partial");
        assert!(!exit.output().unwrap().stdout().complete());
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
    let snapshot = wait_for(&mut state, SessionPhase::Closed).await;
    assert!(snapshot.error().is_none());
    let exit = snapshot.exit().unwrap();
    assert_eq!(exit.report().exit(), Some(ExitStatus::Signaled(15)));
    assert!(exit.output().unwrap().stdout().bytes().is_empty());
    assert!(exit.output().unwrap().stdout().complete());
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
        exit_report(
            &guest,
            &request,
            report,
            OutputStream::new(b"unfinished", false, false).unwrap(),
        );
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    release.send(()).unwrap();
    let snapshot = wait_for(&mut state, SessionPhase::Failed).await;
    assert!(snapshot.error().is_some());
    let exit = snapshot.exit().unwrap();
    assert_eq!(exit.report().cleanup(), CleanupStatus::InProgress);
    assert!(!exit.report().is_final());
    assert_eq!(exit.output().unwrap().stdout().bytes(), b"unfinished");
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
