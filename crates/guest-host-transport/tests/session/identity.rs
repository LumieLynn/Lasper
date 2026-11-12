use super::fixture::{
    command, final_exit, publish_ready, reply, request, Server, BROKER, PREPARATION,
};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{drive_io, snapshot};
use crate::worker::TestWorker;
use lasper_guest_host_transport::{
    BrokerSession, BrokerSessionError, SessionPhase, SessionStartOptions,
};
use lasper_guest_protocol::{
    ExitReason, PreparationControl, PreparationControlKind, StartAcceptedFrame, StartFrame,
};
use std::time::Duration;
use tokio::time::{timeout, Instant};

#[tokio::test]
async fn identity_observation_renews_its_lease_before_the_worker_replies() {
    let server = Server::spawn(|guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 1000);
        worker.wait_for_recheck();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        reply(&guest, keep, 45_000);
        // The original lease must expire while the identity reply is still
        // withheld. Only the confirmed Keep makes the later reply usable.
        std::thread::sleep(Duration::from_millis(1100));
        worker.release_recheck();
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
    });
    let mut session = server.prepare().await;
    timeout(Duration::from_secs(3), session.recheck())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    assert!(session.snapshot().error().is_none());
    session.cancel().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn start_waits_for_the_keep_already_in_flight() {
    start_while_keep_is_pending(false).await;
}

#[tokio::test]
async fn cancelling_start_while_keep_is_pending_preserves_preparation() {
    start_while_keep_is_pending(true).await;
}

async fn start_while_keep_is_pending(cancel: bool) {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (waiting, waited) = tokio::sync::oneshot::channel();
    let (acknowledge, acknowledged) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 1000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        worker.release_recheck();
        waiting.send(()).unwrap();
        acknowledged.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&guest, keep, 45_000);
        if cancel {
            let recheck = command(&guest, &request);
            assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
            reply(&guest, recheck, 45_000);
        } else {
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
            assert!(start.request_id() > keep.request_id());
            let accepted = StartAcceptedFrame::new(
                start.request_id(),
                PreparationControl::new(BROKER, PREPARATION),
            );
            drop(frame);
            let (header, length) = accepted.encode_parts(&mut bytes).unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        }
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let mut session = server.prepare().await;
    tokio::time::pause();
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("identity reply is still gated: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(501)).await;
    drive_io(waited).await.unwrap();
    if cancel {
        drop(start);
        acknowledge.send(()).unwrap();
        drive_io(session.recheck()).await.unwrap();
        assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    } else {
        acknowledge.send(()).unwrap();
        assert!(drive_io(&mut start).await.unwrap().is_none());
        drop(start);
        assert_eq!(session.snapshot().phase(), SessionPhase::Started);
    }
    assert!(session.snapshot().error().is_none());
    drive_io(session.close()).await.unwrap();
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn identity_timeout_keeps_cleanup_within_its_own_budget() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (closing, closed) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 45_000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        closing.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let mut recheck = Box::pin(session.recheck());
    drive_io(async {
        tokio::select! {
            result = &mut recheck => panic!("identity reply is still gated: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(5001)).await;
    assert!(matches!(
        drive_io(&mut recheck).await,
        Err(BrokerSessionError::Deadline)
    ));
    drop(recheck);
    drive_io(closed).await.unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Closing);
    tokio::time::advance(Duration::from_millis(14_999)).await;
    assert_eq!(session.snapshot().phase(), SessionPhase::Closing);
    tokio::time::advance(Duration::from_millis(2)).await;
    let final_state = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert!(matches!(
        final_state.error(),
        Some(BrokerSessionError::Deadline)
    ));
    assert!(final_state.exit().is_none());
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn identity_failure_does_not_discard_an_already_queued_final_exit() {
    worker_exit_during_identity(false, false).await;
}

#[tokio::test]
async fn identity_failure_waits_for_a_later_final_exit() {
    worker_exit_during_identity(true, false).await;
}

#[tokio::test]
async fn identity_failure_keeps_the_final_exit_from_a_closed_control_peer() {
    worker_exit_during_identity(false, true).await;
}

async fn worker_exit_during_identity(later: bool, disconnect: bool) {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (queued, wait) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 44_000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(worker);
        if later {
            queued.send(()).unwrap();
            assert_eq!(
                command(&guest, &request).kind(),
                PreparationControlKind::Close
            );
        }
        final_exit(&guest, &request, ExitReason::WorkerExitedBeforeStart);
        if disconnect {
            drop(guest);
            queued.send(()).unwrap();
            return;
        }
        if !later {
            queued.send(()).unwrap();
        }
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let mut session = server.prepare().await;
    let mut operation = Box::pin(session.recheck());
    timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut operation => panic!("worker is still gated: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    // Both descriptors are ready before the host resumes in the first case.
    wait.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(operation.await.is_err());
    let snapshot = timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = session.snapshot();
            if snapshot.phase() == SessionPhase::Closed {
                break snapshot;
            }
            assert_ne!(snapshot.phase(), SessionPhase::Failed, "{snapshot:?}");
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(snapshot.error(), Some(BrokerSessionError::Io(_))));
    assert_eq!(
        snapshot.exit().unwrap().report().reason(),
        ExitReason::WorkerExitedBeforeStart
    );
    server.finish().await;
}

#[tokio::test]
async fn initial_identity_observation_receives_cleanup_before_its_deadline() {
    let server = Server::spawn(|guest, request| {
        let mut worker =
            TestWorker::pausing_on_first_observation(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        worker.wait_for_recheck();
        final_exit(&guest, &request, ExitReason::SupervisorStopped);
        let _ = receive_guest_frame(&guest, &mut [0; 4096]);
    });
    let result = timeout(
        Duration::from_secs(1),
        BrokerSession::prepare(
            server.connection().await,
            request(),
            Instant::now() + Duration::from_secs(5),
        ),
    )
    .await
    .expect("initial identity hid the guardian's cleanup report");
    assert!(matches!(
        result,
        Err(BrokerSessionError::InvalidState(SessionPhase::Closed))
    ));
    server.finish().await;
}

#[tokio::test]
async fn initial_identity_observation_keeps_the_lease_alive() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let server = Server::spawn(|guest, request| {
        let mut worker =
            TestWorker::pausing_on_first_observation(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 1000, &mut worker);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        reply(&guest, keep, 45_000);
        worker.release_recheck();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let connection = server.connection().await;
    tokio::time::pause();
    let mut prepare = Box::pin(BrokerSession::prepare(
        connection,
        request(),
        Instant::now() + Duration::from_secs(5),
    ));
    drive_io(async {
        tokio::select! {
            result = &mut prepare => panic!("identity reply is still gated: {:?}", result.err()),
            result = checked => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(501)).await;
    let mut session = drive_io(prepare).await.unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    drive_io(session.close()).await.unwrap();
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn cancelling_prepare_during_identity_requests_close() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let server = Server::spawn(|guest, request| {
        let mut worker =
            TestWorker::pausing_on_first_observation(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let mut prepare = Box::pin(BrokerSession::prepare(
        server.connection().await,
        request(),
        Instant::now() + Duration::from_secs(5),
    ));
    drive_io(async {
        tokio::select! {
            result = &mut prepare => panic!("identity reply is still gated: {:?}", result.err()),
            result = checked => result.unwrap(),
        }
    })
    .await;
    drop(prepare);
    server.finish().await;
}

#[tokio::test]
async fn dropping_during_identity_requests_close_without_waiting_for_the_reply() {
    let (checking, checked) = tokio::sync::oneshot::channel();
    let server = Server::spawn(|guest, request| {
        let mut worker = TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        reply(&guest, recheck, 44_000);
        worker.wait_for_recheck();
        checking.send(()).unwrap();
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::Close
        );
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let mut session = server.prepare().await;
    let mut operation = Box::pin(session.recheck());
    drive_io(async {
        tokio::select! {
            result = &mut operation => panic!("worker is still gated: {result:?}"),
            result = checked => result.unwrap(),
        }
    })
    .await;
    drop(operation);
    let mut state = session.subscribe();
    drop(session);
    let snapshot =
        crate::session_clock::snapshot(&mut state, |state| state.phase() == SessionPhase::Closed)
            .await;
    assert!(snapshot.error().is_none());
    server.finish().await;
}
