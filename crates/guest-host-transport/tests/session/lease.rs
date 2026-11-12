use super::fixture::{command, final_exit, publish_ready, ready, reply, Server, PREPARATION};
use crate::control::receive_guest_frame;
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, SessionStartOptions};
use lasper_guest_protocol::{ExitReason, PreparationControlFrame, PreparationControlKind};
use lasper_guest_runtime::transport::TransportError;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn cancelled_recheck_finishes_identity_and_keeps_preparation_usable() {
    cancelled_identity(false).await;
}

#[tokio::test]
async fn cancelled_start_finishes_identity_without_starting_and_renews_its_lease() {
    cancelled_identity(true).await;
}

async fn cancelled_identity(start: bool) {
    let (observing, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (renewed, renewal) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let mut worker =
            crate::worker::TestWorker::pausing_on_recheck(request.request().clone(), PREPARATION);
        publish_ready(&guest, &request, 45_000, &mut worker);
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 1000);
        worker.wait_for_recheck();
        observing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.release_recheck();
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        reply(&guest, keep, 45_000);
        renewed.send(()).unwrap();
        let recheck = command(&guest, &request);
        assert_eq!(recheck.kind(), PreparationControlKind::Recheck);
        reply(&guest, recheck, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        reply(&guest, cancel, 0);
        final_exit(&guest, &request, ExitReason::Cancelled);
    });
    let mut session = server.prepare().await;
    let mut operation = Box::pin(async {
        if start {
            session
                .start(SessionStartOptions::default())
                .await
                .map(|_| ())
        } else {
            session.recheck().await.map(|_| ())
        }
    });
    timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut operation => panic!("identity reply is still gated: {result:?}"),
            result = observed => result.unwrap(),
        }
    })
    .await
    .unwrap();
    drop(operation);
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), renewal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    assert!(session.snapshot().error().is_none());
    session.recheck().await.unwrap();
    session.cancel().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn lease_is_confirmed_while_the_caller_is_idle_and_drop_requests_cleanup() {
    let (renewed, renewal) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 1000);
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        reply(&guest, keep, 45_000);
        renewed.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let session = server.prepare().await;
    timeout(Duration::from_secs(2), renewal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.snapshot().phase(), SessionPhase::Prepared);
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn a_mismatched_lease_reply_invalidates_the_owner_instead_of_renewing() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 1000);
        let keep = command(&guest, &request);
        let wrong =
            PreparationControlFrame::new(keep.request_id() + 1, keep.kind(), keep.control());
        reply(&guest, wrong, 45_000);
    });
    let mut session = server.prepare().await;
    timeout(Duration::from_secs(2), async {
        while session.snapshot().phase() != SessionPhase::Failed {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        session.snapshot().error(),
        Some(BrokerSessionError::Protocol(_))
    ));
    server.finish().await;
}

#[tokio::test]
async fn an_unacknowledged_keep_cannot_extend_the_previous_lease() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 1000);
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::KeepPrepared
        );
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    timeout(Duration::from_secs(3), async {
        while session.snapshot().phase() != SessionPhase::Failed {
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        session.snapshot().error(),
        Some(BrokerSessionError::Deadline)
    ));
    server.finish().await;
}
