use super::fixture::{
    command, exit_report, final_exit, ready, reply, send_guest_bytes, Server, BROKER, PREPARATION,
};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{advance_with_queued_frame, drive_io, snapshot};
use crate::worker::TestWorker;
use lasper_guest_host_transport::{
    BrokerSessionError, SessionPhase, StartError, StartOptions, StartOutcome,
};
use lasper_guest_protocol::{
    CleanupStatus, ErrorFrame, ExecutionStatus, ExitReason, ExitReport, ExitStatus, GuestErrorCode,
    GuestFailure, GuestFailureStage, GuestOperation, PreparationControl, PreparationControlFrame,
    PreparationControlKind, StartAcceptedFrame,
};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

struct WakeOnce<F>(Mutex<Option<F>>);

impl<F: FnOnce() + Send> Wake for WakeOnce<F> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let action = self.0.lock().unwrap().take();
        if let Some(action) = action {
            action();
        }
    }
}

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
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert_eq!(
        failed.report.unwrap().reason(),
        ExitReason::WorkerExitedBeforeStart
    );
    assert!(failed.report.unwrap().is_final());
    server.finish().await;
}

#[tokio::test]
async fn a_starting_write_failure_retains_the_queued_final_exit() {
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        // SAFETY: SHUT_RD rejects Start without discarding the queued Exit
        // on the sending half. The guest retains ownership of the socket.
        assert_eq!(
            unsafe { libc::shutdown(guest.as_raw_fd(), libc::SHUT_RD) },
            0
        );
        sent.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    let mut notifications = session.subscribe();
    notifications.borrow_and_update();
    let mut changed = std::pin::pin!(notifications.changed());
    // The Starting notification runs this waker before the driver can flush
    // Start. Queue Exit and refuse writes at that boundary, not while identity
    // verification is still pending.
    let waker = Waker::from(Arc::new(WakeOnce(Mutex::new(Some(move || {
        release.send(()).unwrap();
        queued.recv_timeout(Duration::from_secs(3)).unwrap();
    })))));
    assert!(changed
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let outcome = session.start(StartOptions::default()).await.unwrap();
    assert!(
        matches!(outcome, StartOutcome::Unknown { ref cause }
        if matches!(cause.as_ref(), BrokerSessionError::Io(error)
            if error.raw_os_error() == Some(libc::EPIPE))),
        "{outcome:?}"
    );
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert!(failed.report.unwrap().is_final());
    finish.send(()).unwrap();
    server.finish().await;
}

#[tokio::test]
async fn an_exit_during_start_identity_verification_prevents_submission() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        super::fixture::publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        // Keep the identity reply blocked until the host handles Exit.
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(lasper_guest_runtime::transport::TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    let mut start = Box::pin(session.start(StartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("identity reply is still blocked: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    release.send(()).unwrap();
    assert!(matches!(
        drive_io(start).await,
        Err(StartError::NotSubmitted(BrokerSessionError::InvalidState(
            SessionPhase::Finished
        )))
    ));
    let state = session.snapshot();
    assert_eq!(state.phase(), SessionPhase::Finished);
    assert!(state.error().is_none());
    let report = state.report.unwrap();
    assert_eq!(report.reason(), ExitReason::WorkerExitedBeforeStart);
    assert!(report.is_final());
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
                ExecutionStatus::Started,
                Some(ExitStatus::Exited(0)),
                CleanupStatus::Complete,
                ExitReason::WorkerExited,
            )
            .unwrap(),
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
    assert!(matches!(
        session.start(StartOptions::default()).await.unwrap(),
        StartOutcome::Accepted
    ));
    assert_eq!(session.snapshot().phase(), SessionPhase::Accepted);
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(session.close().await.is_ok());
    let closed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert!(closed.report.unwrap().is_final());
    assert_eq!(closed.report.unwrap().execution(), ExecutionStatus::Started);
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
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert!(matches!(failed.error(), Some(BrokerSessionError::Io(error))
        if error.kind() == std::io::ErrorKind::InvalidData));
    assert!(failed.report.is_none());
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

#[tokio::test]
async fn an_exit_before_start_ack_retains_the_exec_failure_details() {
    let failure = GuestFailure::new(GuestFailureStage::Exec, 2).unwrap();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        assert_eq!(
            frame.header().kind(),
            lasper_guest_protocol::MessageKind::Start
        );
        drop(frame);
        let report = ExitReport::new(
            ExecutionStatus::Failed { errno: 2 },
            Some(ExitStatus::Exited(126)),
            CleanupStatus::Complete,
            ExitReason::ExecFailed,
        )
        .unwrap()
        .with_failure(Some(failure))
        .unwrap();
        exit_report(&guest, &request, report);
    });
    let mut session = server.prepare().await;
    let outcome = session.start(StartOptions::default()).await.unwrap();
    let StartOutcome::Reported(exit) = outcome else {
        panic!("expected an execution report, got {outcome:?}");
    };
    assert_eq!(exit.failure(), Some(failure));
    assert!(exit.command_exit().is_none());
    server.finish().await;
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
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        if rejected {
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
    let mut start = Box::pin(session.start(StartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("the guest has not replied: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    release.send(()).unwrap();
    queued.recv_timeout(Duration::from_secs(3)).unwrap();
    let result = start.await;
    if rejected {
        assert!(
            matches!(result, Err(StartError::Rejected(ref frame)) if frame.code() == GuestErrorCode::Expired),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(
                result,
                Ok(StartOutcome::Unknown { ref cause }) if matches!(cause.as_ref(), BrokerSessionError::Protocol(_))
            ),
            "{result:?}"
        );
    }
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert!(failed.report.unwrap().is_final());
    assert_eq!(
        failed.report.unwrap().execution(),
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
        for _ in 0..12 {
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
            );
        }
        final_exit(&guest, &request, ExitReason::ClientClosed);
        sent.send(()).unwrap();
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(15_001)).await;
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
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Finished).await;
    assert!(failed.report.unwrap().is_final());
    assert_eq!(failed.report.unwrap().reason(), ExitReason::ClientClosed);
    drive_io(server.finish()).await;
}
