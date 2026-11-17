use super::*;

#[tokio::test]
async fn losing_the_driver_reply_does_not_claim_start_was_not_submitted() {
    let (reply, received) = oneshot::channel();
    let (delivered, _delivery) = oneshot::channel();
    drop(reply);
    assert!(matches!(
        receive(received, delivered).await.unwrap(),
        StartOutcome::Unknown {
            cause
        } if matches!(*cause, BrokerSessionError::DriverStopped)
    ));
}
