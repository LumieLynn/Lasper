use super::fixture::{request, Server, BROKER};
use crate::control::{receive_guest_frame, send_guest_frame};
use lasper_guest_host_transport::{BrokerSession, BrokerSessionError};
use lasper_guest_protocol::{ErrorFrame, GuestErrorCode, GuestOperation};
use lasper_guest_runtime::transport::TransportError;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn the_callers_prepare_deadline_closes_a_connection_waiting_for_ready() {
    let server = Server::spawn(|guest, _| {
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        BrokerSession::prepare(
            server.connection().await,
            request(),
            Instant::now() + Duration::from_millis(100),
        ),
    )
    .await
    .expect("Prepare exceeded its caller's deadline");
    assert!(matches!(result, Err(BrokerSessionError::Deadline)));
    server.finish().await;
}

#[tokio::test]
async fn cancelling_prepare_closes_the_control_connection_before_ready() {
    let (seen, observed) = tokio::sync::oneshot::channel();
    let server = Server::spawn(move |guest, _| {
        seen.send(()).unwrap();
        assert!(matches!(
            receive_guest_frame(&guest, &mut [0; 4096]),
            Err(TransportError::UnexpectedEof)
        ));
    });
    let connection = server.connection().await;
    let mut attempt = Box::pin(BrokerSession::prepare(
        connection,
        request(),
        Instant::now() + Duration::from_secs(10),
    ));
    tokio::select! {
        result = &mut attempt => panic!("unexpected preparation result: {}", result.err().unwrap()),
        _ = observed => {}
    }
    drop(attempt);
    server.finish().await;
}

#[tokio::test]
async fn prepare_rejection_retains_the_guest_error_code() {
    let server = Server::spawn(|guest, request| {
        let error = ErrorFrame::new(
            request.request_id(),
            BROKER,
            None,
            GuestOperation::Prepare,
            GuestErrorCode::Expired,
            None,
        )
        .unwrap();
        let mut bytes = [0; 4096];
        let (header, length) = error.encode_parts(&mut bytes).unwrap();
        send_guest_frame(&guest, header, &bytes[..length], &[]).unwrap();
    });
    let result = BrokerSession::prepare(
        server.connection().await,
        request(),
        Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert!(
        matches!(result, Err(BrokerSessionError::Rejected(error)) if error.code() == GuestErrorCode::Expired)
    );
    server.finish().await;
}
