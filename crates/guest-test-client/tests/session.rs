use super::*;
use std::{cell::Cell, future::pending};
use tokio::sync::oneshot;

#[tokio::test]
async fn output_eof_does_not_disable_stop_while_waiting_for_completion() {
    let (stopped, stop) = oneshot::channel();
    let drain = async {
        stopped.send(()).unwrap();
        Ok(())
    };
    let completion = timeout(
        Duration::from_secs(1),
        observe(
            pending::<u8>(),
            async { 7 },
            async {
                stop.await.unwrap();
            },
            drain,
            false,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(completion, 7);
}

#[tokio::test]
async fn stop_and_start_rejection_both_close_concurrently_with_output() {
    for already_closing in [false, true] {
        let (drained, received) = oneshot::channel();
        let closed = Cell::new(false);
        let close = async {
            received.await.unwrap();
            closed.set(true);
            9
        };
        let drain = async {
            tokio::task::yield_now().await;
            drained.send(()).unwrap();
            Ok(())
        };
        let completion = timeout(
            Duration::from_secs(1),
            observe(pending::<u8>(), close, async {}, drain, already_closing),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(closed.get());
        assert_eq!(completion, 9);
    }
}

#[tokio::test]
async fn output_failure_requests_cleanup_before_returning_the_error() {
    let closed = Cell::new(false);
    let result = observe(
        pending::<()>(),
        async {
            closed.set(true);
        },
        pending::<()>(),
        async { anyhow::bail!("host output failed") },
        false,
    )
    .await;
    assert!(closed.get());
    assert_eq!(result.unwrap_err().to_string(), "host output failed");
}

#[tokio::test]
async fn final_output_is_drained_after_completion() {
    let drained = Cell::new(false);
    let result = observe(
        async { 3 },
        pending::<u8>(),
        pending::<()>(),
        async {
            tokio::task::yield_now().await;
            drained.set(true);
            Ok(())
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(result, 3);
    assert!(drained.get());
}
