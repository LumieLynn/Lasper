use super::fixture::{
    command, exit_report, final_exit, publish_ready, ready, reply, send_guest_bytes,
    send_guest_fragment, Server, BROKER, PREPARATION,
};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{advance_with_queued_frame, assert_expired, drive_io, snapshot};
use crate::worker::TestWorker;
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, StartOptions, StartOutcome};
use lasper_guest_protocol::{
    CleanupStatus, ExecutionStatus, ExitFrame, ExitReason, ExitReport, ExitStatus,
    PreparationControl, PreparationControlKind, StartAcceptedFrame,
};
use std::time::Duration;

#[tokio::test]
async fn keep_expiry_retains_the_final_exit_queued_during_identity() {
    expired_keep(false, false).await;
}

#[tokio::test]
async fn expired_keep_ack_cannot_renew_but_does_not_hide_the_following_exit() {
    expired_keep(true, false).await;
}

#[tokio::test]
async fn expiry_cannot_attach_an_exit_for_another_preparation() {
    expired_keep(false, true).await;
}

async fn expired_keep(with_ack: bool, wrong_preparation: bool) {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (renewing, renewal) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 1000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        renewing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        if with_ack {
            reply(&guest, keep, 45_000);
        }
        if wrong_preparation {
            let report = ExitFrame::new(
                &request,
                lasper_guest_protocol::PreparationId::new([0x9a; 16]),
                ExitReport::new(
                    ExecutionStatus::NotStarted,
                    Some(ExitStatus::Signaled(15)),
                    CleanupStatus::Complete,
                    ExitReason::LeaseExpired,
                )
                .unwrap(),
            )
            .unwrap();
            let mut bytes = [0; 4096];
            let (header, length) = super::fixture::protocol()
                .encode_exit(&report, &mut bytes)
                .unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        } else {
            final_exit(&guest, &request, ExitReason::LeaseExpired);
        }
        sent.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let mut recheck = Box::pin(session.recheck());
    drive_io(async {
        tokio::select! {
            result = &mut recheck => panic!("identity reply is gated: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(501)).await;
    drive_io(renewal).await.unwrap();
    advance_with_queued_frame(Duration::from_millis(500), release, queued).await;
    assert!(matches!(
        drive_io(&mut recheck).await,
        Err(BrokerSessionError::Deadline)
    ));
    drop(recheck);
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert_expired(&failed);
    if wrong_preparation {
        assert!(failed.report.is_none());
    } else {
        assert_eq!(failed.report.unwrap().reason(), ExitReason::LeaseExpired);
        assert!(failed.report.unwrap().is_final());
    }
    assert!(matches!(
        session.recheck().await,
        Err(BrokerSessionError::DriverStopped)
    ));
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn start_ack_expiry_keeps_the_execution_report_without_claiming_acceptance() {
    expired_start(false).await;
}

#[tokio::test]
async fn start_delivery_expiry_keeps_the_execution_report() {
    expired_start(true).await;
}

#[tokio::test]
async fn start_ack_timeout_retains_the_connection_for_late_acceptance_and_close() {
    for late_acceptance in [false, true] {
        let (starting, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let server = Server::spawn(move |guest, request| {
            let _worker = ready(&guest, &request, 45_000);
            reply(&guest, command(&guest, &request), 45_000);
            let mut bytes = [0; 4096];
            let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
            let start_id = frame.header().request_id();
            assert_eq!(
                frame.header().kind(),
                lasper_guest_protocol::MessageKind::Start
            );
            drop(frame);
            starting.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(3)).unwrap();
            if late_acceptance {
                let accepted =
                    StartAcceptedFrame::new(start_id, PreparationControl::new(BROKER, PREPARATION));
                let (header, length) = super::fixture::protocol()
                    .encode_start_accepted(&accepted, &mut bytes)
                    .unwrap();
                send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
            }
            let close = command(&guest, &request);
            assert_eq!(close.kind(), PreparationControlKind::Close);
            reply(&guest, close, 0);
            final_exit(&guest, &request, ExitReason::ClientClosed);
            assert!(receive_guest_frame(&guest, &mut bytes).is_err());
        });
        let mut session = server.prepare().await;
        let mut state = session.subscribe();
        tokio::time::pause();
        let mut start = Box::pin(session.start(StartOptions::default()));
        drive_io(async {
            tokio::select! {
                result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
                result = started => result.unwrap(),
            }
        })
        .await;
        tokio::time::advance(Duration::from_millis(5001)).await;
        assert!(matches!(
            drive_io(&mut start).await,
            Ok(StartOutcome::Unknown { .. })
        ));
        drop(start);
        assert_eq!(session.snapshot().phase(), SessionPhase::Starting);
        assert!(session.snapshot().error().is_none());
        assert!(matches!(
            session.start(StartOptions::default()).await,
            Err(lasper_guest_host_transport::StartError::NotSubmitted(
                BrokerSessionError::InvalidState(SessionPhase::Starting)
            ))
        ));
        release.send(()).unwrap();
        if late_acceptance {
            snapshot(&mut state, |state| state.phase() == SessionPhase::Accepted).await;
        }
        drive_io(session.close()).await.unwrap();
        let completion = drive_io(session.wait()).await;
        assert!(completion.report.unwrap().is_final());
        assert!(completion.error.is_none());
        drive_io(server.finish()).await;
        tokio::time::resume();
    }
}

#[tokio::test]
async fn cleanup_expiry_retains_a_queued_start_ack_and_final_exit() {
    cleanup_expiry_retains_start_reply(false).await;
}

#[tokio::test]
async fn cleanup_expiry_retains_a_queued_start_rejection_and_final_exit() {
    cleanup_expiry_retains_start_reply(true).await;
}

async fn cleanup_expiry_retains_start_reply(rejected: bool) {
    let (starting, started) = tokio::sync::oneshot::channel();
    let (closing, closed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start_id = frame.header().request_id();
        drop(frame);
        starting.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        closing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        let (header, length) = if rejected {
            let error = lasper_guest_protocol::ErrorFrame::new(
                start_id,
                BROKER,
                Some(PREPARATION),
                lasper_guest_protocol::GuestOperation::Start,
                lasper_guest_protocol::GuestErrorCode::InvalidState,
                None,
            )
            .unwrap();
            super::fixture::protocol()
                .encode_error(&error, &mut bytes)
                .unwrap()
        } else {
            let accepted =
                StartAcceptedFrame::new(start_id, PreparationControl::new(BROKER, PREPARATION));
            super::fixture::protocol()
                .encode_start_accepted(&accepted, &mut bytes)
                .unwrap()
        };
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
        sent.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut bytes).is_err());
    });
    let mut session = server.prepare().await;
    tokio::time::pause();
    let mut start = Box::pin(session.start(StartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(5001)).await;
    assert!(matches!(
        drive_io(&mut start).await,
        Ok(StartOutcome::Unknown { .. })
    ));
    drop(start);
    let mut close = Box::pin(session.close());
    drive_io(async {
        tokio::select! {
            result = &mut close => panic!("cleanup is not acknowledged: {result:?}"),
            result = closed => result.unwrap(),
        }
    })
    .await;
    advance_with_queued_frame(Duration::from_millis(15_001), release, queued).await;
    assert!(matches!(
        drive_io(close).await,
        Err(BrokerSessionError::Deadline)
    ));
    let completion = drive_io(session.wait()).await;
    assert_eq!(
        completion.submission,
        if rejected {
            lasper_guest_host_transport::StartSubmission::Rejected
        } else {
            lasper_guest_host_transport::StartSubmission::Accepted
        }
    );
    assert!(completion.report.unwrap().is_final());
    assert!(matches!(
        completion.error.as_deref(),
        Some(BrokerSessionError::Deadline)
    ));
    drive_io(server.finish()).await;
}

async fn expired_start(accepted: bool) {
    let (starting, started) = tokio::sync::oneshot::channel();
    let (release_ack, ack_released) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
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
        drop(frame);
        starting.send(()).unwrap();
        if accepted {
            ack_released.recv_timeout(Duration::from_secs(3)).unwrap();
            let ack = StartAcceptedFrame::new(
                start.request_id(),
                PreparationControl::new(BROKER, PREPARATION),
            );
            let (header, length) = super::fixture::protocol()
                .encode_start_accepted(&ack, &mut bytes)
                .unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        }
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        exit_report(
            &guest,
            &request,
            ExitReport::new(
                ExecutionStatus::Started,
                Some(ExitStatus::Exited(7)),
                CleanupStatus::Complete,
                ExitReason::WorkerExited,
            )
            .unwrap(),
        );
        sent.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let mut start = Box::pin(session.start(StartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    assert_eq!(state.borrow().phase(), SessionPhase::Starting);
    if accepted {
        // Leave Start unpolled once the ack can arrive so its result stays uncollected.
        release_ack.send(()).unwrap();
        snapshot(&mut state, |state| state.phase() == SessionPhase::Accepted).await;
    }
    advance_with_queued_frame(
        Duration::from_millis(if accepted { 15_001 } else { 5001 }),
        release,
        queued,
    )
    .await;
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    if accepted {
        assert_expired(&failed);
    } else {
        assert!(matches!(
            drive_io(&mut start).await,
            Ok(StartOutcome::Unknown { cause }) if matches!(cause.as_ref(), BrokerSessionError::Deadline)
        ));
        assert!(failed.error().is_none());
    }
    drop(start);
    let exit = failed.report.unwrap();
    assert!(exit.is_final());
    assert_eq!(exit.execution(), ExecutionStatus::Started);
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn frame_expiry_reads_an_available_tail_but_does_not_wait_for_one() {
    expired_frame(true).await;
}

#[tokio::test]
async fn frame_expiry_with_an_incomplete_tail_has_no_exit_report() {
    expired_frame(false).await;
}

async fn expired_frame(complete: bool) {
    let (fragment, received) = tokio::sync::oneshot::channel();
    let (renewing, renewal) = tokio::sync::oneshot::channel();
    let (begin, begun) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 12_000);
        begun.recv_timeout(Duration::from_secs(3)).unwrap();
        let exit = ExitFrame::new(
            &request,
            PREPARATION,
            ExitReport::new(
                ExecutionStatus::NotStarted,
                Some(ExitStatus::Signaled(15)),
                CleanupStatus::Complete,
                ExitReason::LeaseExpired,
            )
            .unwrap(),
        )
        .unwrap();
        let mut payload = [0; 4096];
        let (header, length) = super::fixture::protocol()
            .encode_exit(&exit, &mut payload)
            .unwrap();
        let bytes = [header.encode().as_slice(), &payload[..length]].concat();
        send_guest_fragment(&guest, &bytes[..1]);
        fragment.send(()).unwrap();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        renewing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        if complete {
            send_guest_bytes(&guest, &bytes[1..]);
        }
        sent.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    begin.send(()).unwrap();
    drive_io(received).await.unwrap();
    // Tokio rounds timers up to a millisecond, so exactly four seconds can
    // leave Keep pending while the paused clock waits for the peer.
    tokio::time::advance(Duration::from_millis(4001)).await;
    drive_io(renewal).await.unwrap();
    advance_with_queued_frame(Duration::from_millis(5001), release, queued).await;
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert_expired(&failed);
    assert_eq!(failed.report.is_some(), complete);
    drive_io(server.finish()).await;
}
