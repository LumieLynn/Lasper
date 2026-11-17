use super::fixture::{command, final_exit, protocol, ready, reply, Server, BROKER, PREPARATION};
use crate::control::{receive_guest_frame, send_guest_frame};
use crate::session_clock::{drive_io, snapshot};
use lasper_guest_host_transport::{
    BrokerSessionError, ClientTimeouts, SessionPhase, StartOptions, StartOutcome,
};
use lasper_guest_protocol::{
    ExitReason, PreparationControl, PreparationControlKind, StartAcceptedFrame,
};
use std::{future::Future, time::Duration};
use tokio::time::Instant;

#[tokio::test]
async fn public_budgets_allow_prepare_start_and_cleanup_beyond_the_old_defaults() {
    let (preparing, prepared) = tokio::sync::oneshot::channel();
    let (starting, started) = tokio::sync::oneshot::channel();
    let (closing, closed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn(move |guest, request| {
        preparing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        let _worker = ready(&guest, &request, 45_000);
        reply(&guest, command(&guest, &request), 45_000);
        let mut bytes = [0; 4096];
        let frame = receive_guest_frame(&guest, &mut bytes).unwrap();
        let start_id = frame.header().request_id();
        drop(frame);
        starting.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        let accepted =
            StartAcceptedFrame::new(start_id, PreparationControl::new(BROKER, PREPARATION));
        let (header, length) = protocol()
            .encode_start_accepted(&accepted, &mut bytes)
            .unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        closing.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        final_exit(&guest, &request, ExitReason::ClientClosed);
        assert!(receive_guest_frame(&guest, &mut bytes).is_err());
    });
    let client = server
        .client(ClientTimeouts {
            prepare: Duration::from_secs(10),
            operation: Duration::from_secs(10),
            cleanup_observation: Duration::from_secs(20),
            ..Default::default()
        })
        .await;
    tokio::time::pause();
    let mut prepare = Box::pin(client.prepare(spec(), Instant::now() + Duration::from_secs(30)));
    drive_io(async {
        tokio::select! {
            result = &mut prepare => panic!("Ready has not been sent: {:?}", result.err()),
            result = prepared => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_secs(6)).await;
    release.send(()).unwrap();
    let mut prepared = drive_io(prepare).await.unwrap();
    let mut start = Box::pin(prepared.session.start(
        StartOptions::default(),
        Instant::now() + Duration::from_secs(30),
    ));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_secs(6)).await;
    release.send(()).unwrap();
    assert!(matches!(
        drive_io(start).await.unwrap(),
        StartOutcome::Accepted
    ));
    let mut close = Box::pin(prepared.session.close());
    drive_io(async {
        tokio::select! {
            result = &mut close => panic!("cleanup has not completed: {result:?}"),
            result = closed => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_secs(16)).await;
    release.send(()).unwrap();
    let completion = drive_io(close).await;
    assert!(completion.report.unwrap().is_final());
    assert!(completion.error.is_none());
    drop(prepared);
    drive_io(server.finish()).await;
}

fn spec() -> lasper_guest_host_transport::ProcessSpec {
    use lasper_guest_host_transport::{ProcessSpec, Program, Stdio};
    ProcessSpec {
        user: "alice".into(),
        program: Program::Argv(vec!["/bin/true".into()]),
        stdio: Stdio::Piped,
        environment: Default::default(),
    }
}

#[tokio::test]
async fn public_prepare_caller_deadline_stops_waiting_for_ready() {
    let (arriving, arrived) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, _| {
        arriving.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let client = server
        .client(ClientTimeouts {
            prepare: Duration::from_secs(10),
            ..Default::default()
        })
        .await;
    tokio::time::pause();
    let mut prepare = Box::pin(client.prepare(spec(), Instant::now() + Duration::from_millis(100)));
    drive_io(async {
        tokio::select! {
            result = &mut prepare => panic!("Ready has not been sent: {:?}", result.err()),
            result = arrived => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(matches!(
        drive_io(prepare).await,
        Err(lasper_guest_host_transport::PrepareError::Session {
            error: BrokerSessionError::Deadline,
            ..
        })
    ));
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn public_start_caller_deadline_returns_unknown_without_closing_the_connection() {
    let (starting, started) = tokio::sync::oneshot::channel();
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
        starting.send(()).unwrap();
        let close = command(&guest, &request);
        assert_eq!(close.kind(), PreparationControlKind::Close);
        reply(&guest, close, 0);
        final_exit(&guest, &request, ExitReason::ClientClosed);
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let mut prepared = server
        .prepare_client_with_timeouts(ClientTimeouts {
            operation: Duration::from_secs(10),
            ..Default::default()
        })
        .await;
    tokio::time::pause();
    let mut start = Box::pin(prepared.session.start(
        StartOptions::default(),
        Instant::now() + Duration::from_millis(100),
    ));
    drive_io(async {
        tokio::select! {
            result = &mut start => panic!("Start has not been acknowledged: {result:?}"),
            result = started => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(
        matches!(drive_io(start).await, Ok(StartOutcome::Unknown { cause })
        if matches!(cause.as_ref(), BrokerSessionError::Deadline))
    );
    assert_eq!(prepared.session.snapshot().phase, SessionPhase::Starting);
    let completion = drive_io(prepared.session.close()).await;
    assert!(completion.report.unwrap().is_final());
    assert!(completion.error.is_none());
    drop(prepared);
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn public_hello_budget_can_exceed_one_second_and_probe_does_not_prepare() {
    let (arriving, arrived) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = Server::spawn_with_hello(
        super::fixture::request(),
        move |_| {
            arriving.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(3)).unwrap();
            true
        },
        |guest, _| {
            // The fixture gets here only if probe incorrectly sends Prepare.
            drop(guest);
            panic!("probe created a worker");
        },
    );
    let client = server
        .client(ClientTimeouts {
            connect_hello: Duration::from_secs(4),
            ..Default::default()
        })
        .await;
    tokio::time::pause();
    let mut probe = Box::pin(client.probe(Instant::now() + Duration::from_secs(10)));
    drive_io(async {
        tokio::select! {
            result = &mut probe => panic!("Hello has not been answered: {result:?}"),
            result = arrived => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_secs(2)).await;
    release.send(()).unwrap();
    drive_io(probe).await.unwrap();
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn public_probe_respects_a_caller_deadline_shorter_than_its_configured_budget() {
    let (arriving, arrived) = tokio::sync::oneshot::channel();
    let server = Server::spawn_with_hello(
        super::fixture::request(),
        move |guest| {
            arriving.send(()).unwrap();
            assert!(receive_guest_frame(guest, &mut [0; 4096]).is_err());
            false
        },
        |_, _| panic!("expired Hello sent Prepare"),
    );
    let client = server
        .client(ClientTimeouts {
            connect_hello: Duration::from_secs(4),
            ..Default::default()
        })
        .await;
    tokio::time::pause();
    let mut probe = Box::pin(client.probe(Instant::now() + Duration::from_millis(100)));
    drive_io(async {
        tokio::select! {
            result = &mut probe => panic!("Hello has not been answered: {result:?}"),
            result = arrived => result.unwrap(),
        }
    })
    .await;
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(matches!(
        drive_io(probe).await,
        Err(lasper_guest_host_transport::BrokerConnectError::Deadline)
    ));
    drive_io(server.finish()).await;
}

#[tokio::test]
async fn repeated_public_close_does_not_restart_the_configured_observation_budget() {
    let (closing, closed) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, request| {
        let _worker = ready(&guest, &request, 45_000);
        let cancel = command(&guest, &request);
        assert_eq!(cancel.kind(), PreparationControlKind::Cancel);
        reply(&guest, cancel, 0);
        closing.send(()).unwrap();
        assert!(receive_guest_frame(&guest, &mut [0; 4096]).is_err());
    });
    let prepared = server
        .prepare_client_with_timeouts(ClientTimeouts {
            cleanup_observation: Duration::from_secs(3),
            ..Default::default()
        })
        .await;
    let mut state = prepared.session.subscribe();
    tokio::time::pause();
    let mut first = Box::pin(prepared.session.close());
    drive_io(async {
        tokio::select! {
            result = &mut first => panic!("cleanup has not completed: {result:?}"),
            result = closed => result.unwrap(),
        }
    })
    .await;
    snapshot(&mut state, |state| state.phase == SessionPhase::Closing).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let mut second = Box::pin(prepared.session.close());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    tokio::time::advance(Duration::from_millis(1001)).await;
    let (first, second) = drive_io(async { tokio::join!(first, second) }).await;
    for completion in [first, second] {
        assert!(completion.report.is_none());
        assert!(matches!(
            completion.error.as_deref(),
            Some(BrokerSessionError::Deadline)
        ));
    }
    drop(prepared);
    drive_io(server.finish()).await;
}
