use super::fixture::{command, exit_report, final_exit, ready, reply, Server, BROKER, PREPARATION};
use crate::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::{
    BrokerSessionError, SessionPhase, StartError, StartOptions, StartOutcome,
};
use lasper_guest_protocol::{
    CleanupStatus, ErrorFrame, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
    GuestOperation, PreparationControl, PreparationControlFrame, PreparationControlKind,
    StartAcceptedFrame, TerminalSize, MAX_EXECUTION_TIMEOUT_MS,
};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn a_control_reply_in_place_of_start_ack_keeps_the_outcome_unknown() {
    invalid_start_reply(false).await;
}

#[tokio::test]
async fn an_error_for_another_operation_cannot_claim_start_was_rejected() {
    invalid_start_reply(true).await;
}

async fn invalid_start_reply(wrong_error: bool) {
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start = super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        if wrong_error {
            let error = ErrorFrame::new(
                start.request_id(),
                BROKER,
                Some(PREPARATION),
                GuestOperation::Recheck,
                GuestErrorCode::InvalidState,
                None,
            )
            .unwrap();
            let (header, length) = super::fixture::protocol()
                .encode_error(&error, &mut bytes)
                .unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        } else {
            reply(
                &guest,
                PreparationControlFrame::new(
                    start.request_id(),
                    PreparationControlKind::Recheck,
                    PreparationControl::new(BROKER, PREPARATION),
                ),
                45_000,
            );
        }
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(lasper_guest_runtime::transport::TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    let outcome = session.start(StartOptions::default()).await.unwrap();
    assert!(
        matches!(
            outcome,
            StartOutcome::Unknown { ref cause } if matches!(cause.as_ref(), BrokerSessionError::Protocol(_))
        ),
        "{outcome:?}"
    );
    assert_eq!(session.snapshot().phase(), SessionPhase::Finished);
    assert!(matches!(
        session.snapshot().error(),
        Some(BrokerSessionError::Protocol(_))
    ));
    assert!(matches!(
        session.start(StartOptions::default()).await,
        Err(StartError::NotSubmitted(BrokerSessionError::DriverStopped))
    ));
    server.finish().await;
}

#[tokio::test]
async fn a_matching_start_error_remains_a_definite_rejection() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start = super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        let error = ErrorFrame::new(
            start.request_id(),
            BROKER,
            Some(PREPARATION),
            GuestOperation::Start,
            GuestErrorCode::Expired,
            None,
        )
        .unwrap();
        let (header, length) = super::fixture::protocol()
            .encode_error(&error, &mut bytes)
            .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
    });
    let mut session = server.prepare().await;
    assert!(matches!(
        session.start(StartOptions::default()).await,
        Err(StartError::Rejected(error)) if error.code() == GuestErrorCode::Expired
    ));
    server.finish().await;
}

#[tokio::test]
async fn acceptance_is_distinct_from_a_later_exec_failure() {
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start = super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        let accepted = StartAcceptedFrame::new(
            start.request_id(),
            PreparationControl::new(BROKER, PREPARATION),
        );
        let (header, length) = super::fixture::protocol()
            .encode_start_accepted(&accepted, &mut bytes)
            .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        exit_report(
            &guest,
            &request,
            ExitReport::new(
                ExecutionStatus::Failed { errno: 2 },
                Some(ExitStatus::Exited(126)),
                CleanupStatus::Complete,
                ExitReason::ExecFailed,
            )
            .unwrap(),
        );
    });
    let mut session = server.prepare().await;
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    assert_eq!(session.snapshot().phase(), SessionPhase::Accepted);
    assert!(session.snapshot().report.is_none());
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Finished {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let snapshot = session.snapshot();
    let report = snapshot.report.unwrap();
    assert_eq!(report.execution(), ExecutionStatus::Failed { errno: 2 });
    assert_eq!(report.command_exit(), None);
    assert!(snapshot.error().is_none());
    server.finish().await;
}

#[tokio::test]
async fn lost_start_ack_is_unknown_and_the_start_is_never_sent_twice() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        // The host cannot tell whether execution followed this received Start.
    });
    let mut session = server.prepare().await;
    assert!(matches!(
        session.start(StartOptions::default()).await,
        Ok(StartOutcome::Unknown { .. })
    ));
    assert!(matches!(
        session.start(StartOptions::default()).await,
        Err(StartError::NotSubmitted(BrokerSessionError::DriverStopped))
    ));
    server.finish().await;
}

#[tokio::test]
async fn cancelling_committed_start_sends_close_without_waiting_for_ack() {
    let (seen, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start = super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        seen.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(frame);
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        let accepted = StartAcceptedFrame::new(
            start.request_id(),
            PreparationControl::new(BROKER, PREPARATION),
        );
        let (header, length) = super::fixture::protocol()
            .encode_start_accepted(&accepted, &mut bytes)
            .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        reply(&guest, close, 0);
        exit_report(
            &guest,
            &request,
            ExitReport::new(
                ExecutionStatus::Started,
                Some(ExitStatus::Signaled(15)),
                CleanupStatus::Complete,
                ExitReason::ClientClosed,
            )
            .unwrap(),
        );
    });
    let mut session = server.prepare().await;
    let mut start = Box::pin(session.start(StartOptions::default()));
    tokio::select! {
        result = &mut start => panic!("Start should wait for acknowledgment: {result:?}"),
        _ = observed => {},
    }
    drop(start);
    assert_eq!(session.snapshot().phase(), SessionPhase::Starting);
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Finished {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(session.snapshot().error().is_none());
    assert_eq!(
        session.snapshot().report.unwrap().execution(),
        ExecutionStatus::Started
    );
    server.finish().await;
}

#[tokio::test]
async fn final_exit_before_start_ack_returns_the_execution_report() {
    exit_before_start_ack(CleanupStatus::Complete).await;
}

#[tokio::test]
async fn start_ack_after_exit_progress_is_validated_without_reopening_the_session() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start = super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        let accepted = StartAcceptedFrame::new(
            start.request_id(),
            PreparationControl::new(BROKER, PREPARATION),
        );
        let report = ExitReport::new(
            ExecutionStatus::Started,
            Some(ExitStatus::Exited(7)),
            CleanupStatus::InProgress,
            ExitReason::WorkerExited,
        )
        .unwrap();
        exit_report(&guest, &request, report);
        let (header, length) = super::fixture::protocol()
            .encode_start_accepted(&accepted, &mut bytes)
            .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        final_exit(&guest, &request, ExitReason::WorkerExited);
    });
    let mut session = server.prepare().await;
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Reported(_)
    ));
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Finished {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        session.snapshot().error().is_none(),
        "{:?}",
        session.snapshot()
    );
    assert_eq!(
        session.snapshot().submission,
        lasper_guest_host_transport::StartSubmission::Accepted
    );
    server.finish().await;
}

#[tokio::test]
async fn cleanup_before_start_ack_returns_its_report_and_keeps_observing() {
    exit_before_start_ack(CleanupStatus::InProgress).await;
}

async fn exit_before_start_ack(cleanup: CleanupStatus) {
    let (advance, next) = std::sync::mpsc::channel();
    let report = ExitReport::new(
        ExecutionStatus::Started,
        Some(ExitStatus::Exited(7)),
        cleanup,
        ExitReason::WorkerExited,
    )
    .unwrap();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        super::fixture::protocol()
            .decode_start_frame(
                frame.header().request_id(),
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
        exit_report(&guest, &request, report);
        if cleanup != CleanupStatus::Complete {
            next.recv_timeout(Duration::from_secs(3)).unwrap();
            exit_report(
                &guest,
                &request,
                ExitReport::new(
                    report.execution(),
                    report.exit(),
                    CleanupStatus::Complete,
                    report.reason(),
                )
                .unwrap(),
            );
        }
    });
    let mut session = server.prepare().await;
    let outcome = session.start(StartOptions::default()).await.unwrap();
    let StartOutcome::Reported(exit) = outcome else {
        panic!("Exit must retain its execution report: {outcome:?}");
    };
    assert_eq!(exit, report);
    assert!(session.snapshot().error().is_none());
    assert_eq!(
        session.snapshot().phase(),
        if cleanup == CleanupStatus::Complete {
            SessionPhase::Finished
        } else {
            SessionPhase::Closing
        }
    );
    if cleanup != CleanupStatus::Complete {
        advance.send(()).unwrap();
        timeout(Duration::from_secs(3), async {
            while session.snapshot().phase() != SessionPhase::Finished {
                session.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    assert!(session.snapshot().error().is_none());
    server.finish().await;
}

#[tokio::test]
async fn cancelling_start_during_recheck_does_not_execute_the_command() {
    let (seen, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        seen.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&guest, recheck, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
    });
    let mut session = server.prepare().await;
    let mut start = Box::pin(session.start(StartOptions::default()));
    tokio::select! {
        result = &mut start => panic!("Start must wait for Recheck: {result:?}"),
        _ = observed => {}
    }
    drop(start);
    release.send(()).unwrap();
    session.cancel().await.unwrap();
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Finished {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    server.finish().await;
}

#[tokio::test]
async fn invalid_start_options_leave_the_prepared_worker_usable() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
    });
    let mut session = server.prepare().await;
    for options in [
        StartOptions {
            terminal_size: Some(TerminalSize::new(80, 24).unwrap()),
            ..Default::default()
        },
        StartOptions {
            execution_timeout: Some(Duration::ZERO),
            ..Default::default()
        },
        StartOptions {
            execution_timeout: Some(
                Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS) + Duration::from_nanos(1),
            ),
            ..Default::default()
        },
        StartOptions {
            execution_timeout: Some(Duration::MAX),
            ..Default::default()
        },
    ] {
        let result = session.start(options).await;
        assert!(matches!(
            result,
            Err(StartError::NotSubmitted(
                BrokerSessionError::InvalidRequest(_)
            ))
        ));
        assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    }
    session.cancel().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn start_forwards_only_the_requested_execution_budget() {
    for (duration, expected) in [
        (None, None),
        (Some(Duration::from_secs(180)), Some(180_000)),
        (Some(Duration::from_nanos(1)), Some(1)),
        (
            Some(Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS)),
            Some(MAX_EXECUTION_TIMEOUT_MS),
        ),
    ] {
        let server = Server::spawn(move |guest, request| {
            let _worker = ready(&guest, &request, 45_000);
            let recheck = command(&guest, &request);
            assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
            reply(&guest, recheck, 45_000);
            let mut bytes = [0; 4096];
            let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
            let start = super::fixture::protocol()
                .decode_start_frame(
                    frame.header().request_id(),
                    &request,
                    PREPARATION,
                    frame.header(),
                    frame.payload(),
                    frame.descriptors().len(),
                )
                .unwrap();
            assert_eq!(start.request().execution_timeout_ms(), expected);
            let accepted = StartAcceptedFrame::new(
                start.request_id(),
                PreparationControl::new(BROKER, PREPARATION),
            );
            let (header, length) = super::fixture::protocol()
                .encode_start_accepted(&accepted, &mut bytes)
                .unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
            let close = command(&guest, &request);
            assert_eq!(close.kind(), PreparationControlKind::Close);
            reply(&guest, close, 0);
            exit_report(
                &guest,
                &request,
                ExitReport::new(
                    ExecutionStatus::Started,
                    Some(ExitStatus::Signaled(15)),
                    CleanupStatus::Complete,
                    ExitReason::ClientClosed,
                )
                .unwrap(),
            );
        });
        let mut session = server.prepare().await;
        assert!(matches!(
            session
                .start(StartOptions {
                    execution_timeout: duration,
                    ..Default::default()
                })
                .await
                .unwrap(),
            StartOutcome::Accepted
        ));
        session.close().await.unwrap();
        server.finish().await;
    }
}
