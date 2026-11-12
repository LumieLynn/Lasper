use super::fixture::{
    command, exit_report, final_exit, publish_ready, ready, reply, send_guest_bytes, Server,
    BROKER, PREPARATION,
};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{advance_with_queued_frame, assert_expired, drive_io, snapshot};
use crate::worker::TestWorker;
use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, SessionStartOptions};
use lasper_guest_protocol::{
    CapturedOutput, CleanupStatus, ExecutionStatus, ExitFrame, ExitReason, ExitReport, ExitStatus,
    OutputStream, PreparationControl, PreparationControlKind, StartAcceptedFrame, StartFrame,
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
            let empty = OutputStream::new(&[], false, true).unwrap();
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
                Some(CapturedOutput::new(empty, empty).unwrap()),
            )
            .unwrap();
            let mut bytes = [0; 4096];
            let (header, length) = report.encode_parts(&mut bytes).unwrap();
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
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&failed);
    if wrong_preparation {
        assert!(failed.exit().is_none());
    } else {
        assert_eq!(
            failed.exit().unwrap().report().reason(),
            ExitReason::LeaseExpired
        );
        assert!(failed.exit().unwrap().report().is_final());
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

async fn expired_start(accepted: bool) {
    let (starting, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (sent, queued) = std::sync::mpsc::channel();
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
        if accepted {
            drop(frame);
            let ack = StartAcceptedFrame::new(
                start.request_id(),
                PreparationControl::new(BROKER, PREPARATION),
            );
            let (header, length) = ack.encode_parts(&mut bytes).unwrap();
            send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        }
        starting.send(()).unwrap();
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
            OutputStream::new(b"finished", false, true).unwrap(),
        );
        sent.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let mut session = server.prepare().await;
    let mut state = session.subscribe();
    tokio::time::pause();
    let mut start = Box::pin(session.start(SessionStartOptions::default()));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    if accepted {
        snapshot(&mut state, |state| state.phase() == SessionPhase::Started).await;
    }
    advance_with_queued_frame(
        Duration::from_millis(if accepted { 15_001 } else { 5001 }),
        release,
        queued,
    )
    .await;
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    if accepted {
        assert_expired(&failed);
    } else {
        assert!(
            matches!(drive_io(&mut start).await, Err(BrokerSessionError::StartOutcomeUnknown(cause)) if matches!(cause.as_ref(), BrokerSessionError::Deadline))
        );
        assert!(matches!(
            failed.error(),
            Some(BrokerSessionError::StartOutcomeUnknown(_))
        ));
    }
    drop(start);
    let exit = failed.exit().unwrap();
    assert!(exit.report().is_final());
    assert_eq!(exit.report().execution(), ExecutionStatus::Started);
    assert_eq!(exit.output().unwrap().stdout().bytes(), b"finished");
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
        let _worker = ready(&guest, &request, 45_000);
        begun.recv_timeout(Duration::from_secs(3)).unwrap();
        let empty = OutputStream::new(&[], false, true).unwrap();
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
            Some(CapturedOutput::new(empty, empty).unwrap()),
        )
        .unwrap();
        let mut payload = [0; 4096];
        let (header, length) = exit.encode_parts(&mut payload).unwrap();
        let bytes = [header.encode().as_slice(), &payload[..length]].concat();
        send_guest_bytes(&guest, &bytes[..1]);
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
    tokio::task::yield_now().await;
    // A Keep exchange after the fragment ensures its first byte has been
    // processed before advancing across the independent frame deadline.
    tokio::time::advance(Duration::from_secs(10)).await;
    drive_io(renewal).await.unwrap();
    advance_with_queued_frame(Duration::from_millis(5001), release, queued).await;
    let failed = snapshot(&mut state, |state| state.phase() == SessionPhase::Failed).await;
    assert_expired(&failed);
    assert_eq!(failed.exit().is_some(), complete);
    drive_io(server.finish()).await;
}
