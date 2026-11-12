use super::fixture::{command, final_exit, ready, reply, send_guest_bytes, Server, PREPARATION};
use lasper_guest_host_transport::SessionPhase;
use lasper_guest_protocol::{
    CapturedOutput, CleanupStatus, ControlReplyFrame, ControlReplyStatus, ExecutionStatus,
    ExitFrame, ExitReason, ExitReport, OutputStream, PreparationControlKind,
    CONTROL_FRAME_HEADER_BYTES,
};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn cancel_is_sent_while_a_keep_reply_has_only_its_first_header_byte() {
    cleanup_with_partial_reply(PreparationControlKind::Cancel, 1).await;
}

#[tokio::test]
async fn close_is_sent_while_a_keep_reply_has_an_incomplete_payload() {
    cleanup_with_partial_reply(
        PreparationControlKind::Close,
        CONTROL_FRAME_HEADER_BYTES + 2,
    )
    .await;
}

async fn cleanup_with_partial_reply(kind: PreparationControlKind, prefix: usize) {
    let (fragment, received) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 4000);
        let keep = command(&guest, &request);
        assert_eq!(keep.kind(), PreparationControlKind::KeepPrepared);
        let mut payload = [0; 512];
        let (header, length) = ControlReplyFrame::new(
            keep,
            ControlReplyStatus::Prepared {
                remaining_lease_ms: 45_000,
            },
        )
        .unwrap()
        .encode_parts(&mut payload)
        .unwrap();
        let bytes = [header.encode().as_slice(), &payload[..length]].concat();
        send_guest_bytes(&guest, &bytes[..prefix]);
        fragment.send(()).unwrap();

        // Withhold the rest of the reply until cleanup arrives. A host that
        // waits for a whole reply before sending cleanup will stall here.
        let cleanup = command(&guest, &request);
        assert_eq!(cleanup.kind(), kind);
        send_guest_bytes(&guest, &bytes[prefix..]);
        reply(&guest, cleanup, 0);
        final_exit(
            &guest,
            &request,
            match kind {
                PreparationControlKind::Cancel => ExitReason::Cancelled,
                _ => ExitReason::ClientClosed,
            },
        );
    });
    let mut session = server.prepare().await;
    timeout(Duration::from_secs(3), received)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(2), async {
        match kind {
            PreparationControlKind::Cancel => session.cancel().await,
            _ => session.close().await,
        }
    })
    .await
    .expect("cleanup was blocked by a partial reply")
    .unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            let state = session.snapshot();
            assert!(state.error().is_none(), "{:?}", state.error());
            assert_ne!(state.phase(), SessionPhase::Prepared);
            if state.phase() == SessionPhase::Closed {
                break;
            }
            session.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    server.finish().await;
}

#[tokio::test]
async fn a_partial_notification_does_not_block_lease_renewal() {
    let server = Server::spawn(|guest, request| {
        let _worker = ready(&guest, &request, 1000);
        let report = ExitReport::new(
            ExecutionStatus::NotStarted,
            None,
            CleanupStatus::InProgress,
            ExitReason::LeaseExpired,
        )
        .unwrap();
        let empty = OutputStream::new(&[], false, false).unwrap();
        let exit = ExitFrame::new(
            &request,
            PREPARATION,
            report,
            Some(CapturedOutput::new(empty, empty).unwrap()),
        )
        .unwrap();
        let mut payload = [0; 4096];
        let (header, length) = exit.encode_parts(&mut payload).unwrap();
        let bytes = [header.encode().as_slice(), &payload[..length]].concat();
        send_guest_bytes(&guest, &bytes[..1]);
        assert_eq!(
            command(&guest, &request).kind(),
            PreparationControlKind::KeepPrepared
        );
        send_guest_bytes(&guest, &bytes[1..]);
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
    assert!(session.snapshot().error().is_none());
    server.finish().await;
}
