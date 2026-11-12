use super::fixture::{
    command, exit_report, final_exit, ready, reply, send_guest_bytes, Server, BROKER, PREPARATION,
};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{advance_with_queued_frame, drive_io, snapshot};
use crate::worker::TestWorker;
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, SessionStartOptions};
use lasper_guest_protocol::{
    CleanupStatus, ErrorFrame, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
    GuestOperation, OutputStream, PreparationControl, PreparationControlFrame,
    PreparationControlKind, StartAcceptedFrame, StartFrame,
};
use std::time::Duration;

#[tokio::test]
async fn a_prepared_write_failure_retains_the_queued_final_exit() {
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        // Refuse the next request while leaving the queued Exit readable.
        // SAFETY: guest owns the socket; shutdown does not transfer its FD.
        assert_eq!(
            unsafe { libc::shutdown(guest.as_raw_fd(), libc::SHUT_RD) },
            0
        );
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    let error = session.recheck().await.unwrap_err();
    assert!(
        matches!(
            error,
            BrokerSessionError::Io(ref cause) if cause.raw_os_error() == Some(libc::EPIPE)
        ),
        "{error}"
    );
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_eq!(
        failed.exit().unwrap().report().reason(),
        ExitReason::WorkerExitedBeforeStart
    );
    assert!(failed.exit().unwrap().report().is_final());
    server.finish().await;
}

#[tokio::test]
async fn a_starting_write_failure_retains_the_queued_final_exit() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        super::fixture::publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.release_recheck();
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        // SAFETY: only the guest's receiving half is closed; its reply and
        // Exit remain queued for the host while the Start write fails.
        assert_eq!(
            unsafe { libc::shutdown(guest.as_raw_fd(), libc::SHUT_RD) },
            0
        );
        sent.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Recheck has no reply yet: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    let error = start.await.unwrap_err();
    assert!(
        matches!(error, BrokerSessionError::StartOutcomeUnknown(ref cause)
        if matches!(cause.as_ref(), BrokerSessionError::Io(error)
            if error.raw_os_error() == Some(libc::EPIPE))),
        "{error}"
    );
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert!(failed.exit().unwrap().report().is_final());
    finish.send(()).unwrap();
    server.finish().await;
}

#[tokio::test]
async fn a_started_peer_rejection_does_not_lose_the_final_exit_on_close() {
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
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
        drop(frame);
        let (header, length) = StartAcceptedFrame::new(
            start.request_id(),
            PreparationControl::new(BROKER, PREPARATION),
        )
        .encode_parts(&mut bytes)
        .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        exit_report(
            &guest,
            &request,
            ExitReport::new(
                ExecutionStatus::Started,
                Some(ExitStatus::Exited(0)),
                CleanupStatus::Complete,
                ExitReason::WorkerExited,
            )
            .unwrap(),
            OutputStream::new(b"finished", false, true).unwrap(),
        );
        // SAFETY: the stream stays owned and its send queue remains readable.
        assert_eq!(
            unsafe { libc::shutdown(guest.as_raw_fd(), libc::SHUT_RD) },
            0
        );
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    session.start(SessionStartOptions::default()).await.unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Started);
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(session.close().await.is_ok());
    let closed = snapshot(&mut state, |state| state.phase() == SessionPhase::Closed).await;
    assert!(closed.exit().unwrap().report().is_final());
    assert_eq!(
        closed.exit().unwrap().report().execution(),
        ExecutionStatus::Started
    );
    assert!(matches!(closed.error(), Some(BrokerSessionError::Io(error))
        if error.raw_os_error() == Some(libc::EPIPE)));
    server.finish().await;
}

#[tokio::test]
async fn a_corrupted_header_does_not_allow_scanning_for_a_later_exit() {
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        send_guest_bytes(
            &guest,
            &[0; lasper_guest_protocol::CONTROL_FRAME_HEADER_BYTES],
        );
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert!(matches!(failed.error(), Some(BrokerSessionError::Io(error))
        if error.kind() == std::io::ErrorKind::InvalidData));
    assert!(failed.exit().is_none());
    server.finish().await;
}

#[tokio::test]
async fn an_invalid_start_reply_does_not_hide_the_queued_execution_report() {
    failed_start_reply(false).await;
}

#[tokio::test]
async fn a_rejected_start_preserves_its_error_and_the_queued_final_exit() {
    failed_start_reply(true).await;
}

async fn failed_start_reply(rejected: bool) {
    let (starting, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
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
        drop(frame);
        starting.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        if rejected {
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
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("the guest has not replied: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    let error = start.await.unwrap_err();
    if rejected {
        assert!(
            matches!(error, BrokerSessionError::Rejected(ref frame) if frame.code() == GuestErrorCode::Expired),
            "{error}"
        );
    } else {
        assert!(
            matches!(error, BrokerSessionError::StartOutcomeUnknown(ref cause) if matches!(cause.as_ref(), BrokerSessionError::Protocol(_))),
            "{error}"
        );
    }
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert!(failed.exit().unwrap().report().is_final());
    assert_eq!(
        failed.exit().unwrap().report().execution(),
        ExecutionStatus::NotStarted
    );
    server.finish().await;
}

#[tokio::test]
async fn expiry_retains_a_final_exit_after_both_replies_and_many_progress_reports() {
    let (renewing, renewal) = tokio::sync::oneshot::channel();
    let (closing, closed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        renewing.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        closing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&guest, keep, 45_000);
        reply(&guest, close, 0);
        for step in 0..12 {
            let output = format!("cleanup step {step}");
            exit_report(
                &guest,
                &request,
                ExitReport::new(
                    ExecutionStatus::NotStarted,
                    None,
                    CleanupStatus::InProgress,
                    ExitReason::ClientClosed,
                )
                .unwrap(),
                OutputStream::new(output.as_bytes(), false, false).unwrap(),
            );
        }
        final_exit(&guest, &request, ExitReason::ClientClosed);
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(10_001)).await;
    drive_io(renewal).await.unwrap();
    let mut close = Box::pin(session.close());
    drive_io(async {
        tokio::select! {
            result = &mut close => panic!("cleanup is not acknowledged: {result:?}"),
            result = closed => result.unwrap(),
        }
    })
    .await;
    advance_with_queued_frame(Duration::from_millis(15_001), release, queued).await;
    assert!(matches!(close.await, Err(BrokerSessionError::Deadline)));
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert!(failed.exit().unwrap().report().is_final());
    assert_eq!(
        failed.exit().unwrap().report().reason(),
        ExitReason::ClientClosed
    );
    drive_io(server.finish()).await;
}
