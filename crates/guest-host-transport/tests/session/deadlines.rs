use super::fixture::{command, exit_report, final_exit, ready, reply, send_guest_bytes, Server};
use crate::control::receive_guest_frame;
use crate::session_clock::{advance_with_queued_frame, assert_expired, drive_io, snapshot};
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase};
use lasper_guest_protocol::{
    CapturedOutput, CleanupStatus, ExecutionStatus, ExitFrame, ExitReason, ExitReport,
    OutputStream, PreparationControlKind,
};
use lasper_guest_runtime::transport::{GuestControlStream, TransportError};
use std::time::Duration;

fn assert_peer_closed(guest: &GuestControlStream) {
    let error = receive_guest_frame(guest, &mut [0; 4096])
        .err()
        .expect("the expired owner must close its connection");
    // Closing with unread frame bytes can reset the Unix stream instead of EOF.
    assert!(
        matches!(
            error,
            TransportError::UnexpectedEof | TransportError::Syscall(libc::ECONNRESET)
        ),
        "unexpected disconnect: {error:?}"
    );
}

#[tokio::test]
async fn close_ack_before_the_deadline_leaves_cleanup_pending() {
    ack_before_deadline(PreparationControlKind::Close).await;
}

#[tokio::test]
async fn cancel_ack_before_the_deadline_leaves_cleanup_pending() {
    ack_before_deadline(PreparationControlKind::Cancel).await;
}

async fn ack_before_deadline(kind: PreparationControlKind) {
    let (seen, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cleanup = command(&guest, &request);
        assert_eq!(cleanup.kind(), kind);
        seen.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&guest, cleanup, 0);
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let cleanup = async {
        match kind {
            PreparationControlKind::Close => session.close().await,
            _ => session.cancel().await,
        }
    };
    tokio::pin!(cleanup);
    drive_io(async {
        tokio::select! {
            result = &mut cleanup => panic!("cleanup has not been acknowledged: {result:?}"),
            result = observed => result.unwrap(),
        }
    })
    .await;
    assert_eq!(state.borrow().phase(), SessionPhase::Closing);
    tokio::time::advance(Duration::from_millis(14_999)).await;
    assert_eq!(state.borrow().phase(), SessionPhase::Closing);
    release.send(()).unwrap();
    drive_io(&mut cleanup).await.unwrap();
    assert_eq!(state.borrow().phase(), SessionPhase::Closing);
    assert!(state.borrow().exit().is_none());
    assert!(state.borrow().error().is_none());
    // Tokio rounds timer wakeups to milliseconds; cross that boundary too.
    tokio::time::advance(Duration::from_millis(2)).await;
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    assert!(result.exit().is_none());
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn close_ack_at_the_deadline_is_rejected() {
    expired_ack(PreparationControlKind::Close, Duration::from_secs(15)).await;
}

#[tokio::test]
async fn close_ack_after_the_deadline_is_rejected() {
    expired_ack(PreparationControlKind::Close, Duration::from_millis(15_001)).await;
}

#[tokio::test]
async fn cancel_ack_at_the_deadline_is_rejected() {
    expired_ack(PreparationControlKind::Cancel, Duration::from_secs(15)).await;
}

#[tokio::test]
async fn cancel_ack_after_the_deadline_is_rejected() {
    expired_ack(
        PreparationControlKind::Cancel,
        Duration::from_millis(15_001),
    )
    .await;
}

async fn expired_ack(kind: PreparationControlKind, elapsed: Duration) {
    let (seen, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cleanup = command(&guest, &request);
        assert_eq!(cleanup.kind(), kind);
        seen.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&guest, cleanup, 0);
        sent.send(()).unwrap();
        assert_peer_closed(&guest);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let cleanup = async {
        match kind {
            PreparationControlKind::Close => session.close().await,
            _ => session.cancel().await,
        }
    };
    tokio::pin!(cleanup);
    drive_io(async {
        tokio::select! {
            result = &mut cleanup => panic!("cleanup has not been acknowledged: {result:?}"),
            result = observed => result.unwrap(),
        }
    })
    .await;
    advance_with_queued_frame(elapsed, release, queued).await;
    assert!(matches!(
        drive_io(&mut cleanup).await,
        Err(BrokerSessionError::Deadline)
    ));
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    assert!(result.exit().is_none());
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn unsolicited_in_progress_followed_by_silence_expires() {
    silent_report(CleanupStatus::InProgress).await;
}

#[tokio::test]
async fn unsolicited_pending_followed_by_silence_expires() {
    silent_report(CleanupStatus::Pending {
        errno: Some(libc::EBUSY),
    })
    .await;
}

async fn silent_report(cleanup: CleanupStatus) {
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        exit_report(
            &guest,
            &request,
            ExitReport::new(
                ExecutionStatus::NotStarted,
                None,
                cleanup,
                ExitReason::LeaseExpired,
            )
            .unwrap(),
            OutputStream::new(b"unfinished", false, false).unwrap(),
        );
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    release.send(()).unwrap();
    snapshot(&mut state, |state| state.exit().is_some()).await;
    tokio::time::advance(Duration::from_secs(14)).await;
    assert_eq!(state.borrow().phase(), SessionPhase::Closing);
    tokio::time::advance(Duration::from_millis(1001)).await;
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    let exit = result.exit().unwrap();
    assert_eq!(exit.report().cleanup(), cleanup);
    assert!(!exit.report().is_final());
    assert_eq!(exit.output().unwrap().stdout().bytes(), b"unfinished");
    drop(session);
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn repeated_reports_and_handle_drop_cannot_extend_cleanup_observation() {
    let (next, steps) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        for (cleanup, output) in [
            (CleanupStatus::InProgress, &b"first"[..]),
            (
                CleanupStatus::Pending {
                    errno: Some(libc::EBUSY),
                },
                &b"second"[..],
            ),
            (CleanupStatus::InProgress, &b"third"[..]),
        ] {
            steps.recv_timeout(Duration::from_secs(3)).unwrap();
            exit_report(
                &guest,
                &request,
                ExitReport::new(
                    ExecutionStatus::NotStarted,
                    None,
                    cleanup,
                    ExitReason::ClientClosed,
                )
                .unwrap(),
                OutputStream::new(output, false, false).unwrap(),
            );
        }
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    drive_io(session.close()).await.unwrap();
    for output in [
        b"first".as_slice(),
        b"second".as_slice(),
        b"third".as_slice(),
    ] {
        if output != b"first" {
            tokio::time::advance(Duration::from_secs(7)).await;
        }
        next.send(()).unwrap();
        let result = snapshot(&mut state, |state| {
            state
                .exit()
                .is_some_and(|exit| exit.output().unwrap().stdout().bytes() == output)
        })
        .await;
        assert_eq!(result.phase(), SessionPhase::Closing);
        assert!(result.error().is_none());
    }
    drop(session);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    let exit = result.exit().unwrap();
    assert_eq!(exit.report().cleanup(), CleanupStatus::InProgress);
    assert_eq!(exit.output().unwrap().stdout().bytes(), b"third");
    assert!(!exit.output().unwrap().stdout().complete());
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn a_partial_exit_at_cleanup_expiry_is_discarded() {
    let (next, steps) = std::sync::mpsc::channel();
    let (sent, fragment) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let close = command(&guest, &request);
        reply(&guest, close, 0);
        steps.recv_timeout(Duration::from_secs(3)).unwrap();
        let empty = OutputStream::new(&[], false, false).unwrap();
        let exit = ExitFrame::new(
            &request,
            super::fixture::PREPARATION,
            ExitReport::new(
                ExecutionStatus::NotStarted,
                None,
                CleanupStatus::Pending { errno: None },
                ExitReason::ClientClosed,
            )
            .unwrap(),
            Some(CapturedOutput::new(empty, empty).unwrap()),
        )
        .unwrap();
        let (header, _) = exit.encode_parts(&mut [0; 4096]).unwrap();
        send_guest_bytes(&guest, &header.encode()[..1]);
        sent.send(()).unwrap();
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    drive_io(session.close()).await.unwrap();
    tokio::time::advance(Duration::from_secs(14)).await;
    next.send(()).unwrap();
    drive_io(fragment).await.unwrap();
    tokio::time::advance(Duration::from_millis(1001)).await;
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    assert!(result.exit().is_none(), "a fragment cannot become a report");
    drop(session);
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn a_final_exit_available_at_cleanup_expiry_is_retained_without_reopening_the_session() {
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let close = command(&guest, &request);
        reply(&guest, close, 0);
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
            OutputStream::new(b"pending", false, false).unwrap(),
        );
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        final_exit(&guest, &request, ExitReason::ClientClosed);
        sent.send(()).unwrap();
        assert_peer_closed(&guest);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    drive_io(session.close()).await.unwrap();
    snapshot(&mut state, |state| state.exit().is_some()).await;
    advance_with_queued_frame(Duration::from_millis(15_001), release, queued).await;
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&result);
    let exit = result.exit().unwrap();
    assert_eq!(exit.report().cleanup(), CleanupStatus::Complete);
    assert!(exit.output().unwrap().stdout().bytes().is_empty());
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_expired(&state.borrow());
    drop(session);
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn a_final_report_before_the_deadline_stays_closed_after_it() {
    let (next, steps) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let close = command(&guest, &request);
        reply(&guest, close, 0);
        steps.recv_timeout(Duration::from_secs(3)).unwrap();
        final_exit(&guest, &request, ExitReason::ClientClosed);
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    drive_io(session.close()).await.unwrap();
    tokio::time::advance(Duration::from_secs(14)).await;
    next.send(()).unwrap();
    let result = snapshot(&mut state, |state| state.phase() == SessionPhase::Closed).await;
    assert!(result.exit().unwrap().report().is_final());
    assert!(result.error().is_none());
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_eq!(state.borrow().phase(), SessionPhase::Closed);
    assert!(state.borrow().error().is_none());
    drop(session);
    drive_io(server.finish()).await;
}
