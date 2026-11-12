use super::fixture::{command, exit_report, final_exit, ready, reply, Server, BROKER, PREPARATION};
use crate::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, SessionStartOptions};
use lasper_guest_protocol::{
    CleanupStatus, ErrorFrame, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
    GuestOperation, OutputStream, PreparationControl, PreparationControlFrame,
    PreparationControlKind, StartAcceptedFrame, StartFrame, TerminalSize,
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
        let start = StartFrame::decode_for(
            frame.header().request_id(),
            &request,
            PREPARATION,
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )
        .unwrap();
        if wrong_error {
            let (header, length) = ErrorFrame::new(
                start.request_id(),
                BROKER,
                Some(PREPARATION),
                GuestOperation::Recheck,
                GuestErrorCode::InvalidState,
                None,
            )
            .unwrap()
            .encode_parts(&mut bytes)
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
    let error = session
        .start(SessionStartOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            BrokerSessionError::StartOutcomeUnknown(ref cause)
                if matches!(cause.as_ref(), BrokerSessionError::Protocol(_))
        ),
        "{error}"
    );
    assert_eq!(session.snapshot().phase(), SessionPhase::Failed);
    assert!(matches!(
        session.snapshot().error(),
        Some(BrokerSessionError::StartOutcomeUnknown(_))
    ));
    assert!(matches!(
        session.start(SessionStartOptions::default()).await,
        Err(BrokerSessionError::DriverStopped)
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
        let start = StartFrame::decode_for(
            frame.header().request_id(),
            &request,
            PREPARATION,
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )
        .unwrap();
        let (header, length) = ErrorFrame::new(
            start.request_id(),
            BROKER,
            Some(PREPARATION),
            GuestOperation::Start,
            GuestErrorCode::Expired,
            None,
        )
        .unwrap()
        .encode_parts(&mut bytes)
        .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
    });
    let mut session = server.prepare().await;
    assert!(matches!(
        session.start(SessionStartOptions::default()).await,
        Err(BrokerSessionError::Rejected(error)) if error.code() == GuestErrorCode::Expired
    ));
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
        StartFrame::decode_for(
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
        session.start(SessionStartOptions::default()).await,
        Err(BrokerSessionError::StartOutcomeUnknown(_))
    ));
    assert!(matches!(
        session.start(SessionStartOptions::default()).await,
        Err(BrokerSessionError::DriverStopped)
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
        let start = StartFrame::decode_for(
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
        let (header, length) = accepted.encode_parts(&mut bytes).unwrap();
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
            OutputStream::new(&[], false, true).unwrap(),
        );
    });
    let mut session = server.prepare().await;
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    tokio::select! {
        result = &mut start => panic!("Start should wait for acknowledgment: {result:?}"),
        _ = observed => {},
    }
    drop(start);
    assert_eq!(session.snapshot().phase(), SessionPhase::Starting);
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Closed {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(session.snapshot().error().is_none());
    assert_eq!(
        session.snapshot().exit().unwrap().report().execution(),
        ExecutionStatus::Started
    );
    server.finish().await;
}

#[tokio::test]
async fn final_exit_before_start_ack_returns_the_execution_report() {
    exit_before_start_ack(CleanupStatus::Complete).await;
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
        StartFrame::decode_for(
            frame.header().request_id(),
            &request,
            PREPARATION,
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )
        .unwrap();
        exit_report(
            &guest,
            &request,
            report,
            OutputStream::new(b"command ran", false, true).unwrap(),
        );
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
                OutputStream::new(b"command ran", false, true).unwrap(),
            );
        }
    });
    let mut session = server.prepare().await;
    let error = session
        .start(SessionStartOptions::default())
        .await
        .unwrap_err();
    let BrokerSessionError::StartInterrupted(exit) = error else {
        panic!("Exit must preserve execution uncertainty and its report: {error}");
    };
    assert_eq!(exit.report(), report);
    assert_eq!(exit.output().unwrap().stdout().bytes(), b"command ran");
    assert!(session.snapshot().error().is_none());
    assert_eq!(
        session.snapshot().phase(),
        if cleanup == CleanupStatus::Complete {
            SessionPhase::Closed
        } else {
            SessionPhase::Closing
        }
    );
    if cleanup != CleanupStatus::Complete {
        advance.send(()).unwrap();
        timeout(Duration::from_secs(3), async {
            while session.snapshot().phase() != SessionPhase::Closed {
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
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    tokio::select! {
        result = &mut start => panic!("Start must wait for Recheck: {result:?}"),
        _ = observed => {}
    }
    drop(start);
    release.send(()).unwrap();
    session.cancel().await.unwrap();
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Closed {
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
    let result = session
        .start(SessionStartOptions {
            terminal_size: Some(TerminalSize::new(80, 24).unwrap()),
            ..Default::default()
        })
        .await;
    assert!(matches!(result, Err(BrokerSessionError::InvalidRequest(_))));
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    session.cancel().await.unwrap();
    server.finish().await;
}
