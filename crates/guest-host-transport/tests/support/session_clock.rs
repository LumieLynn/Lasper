use lasper_guest_host_transport::{BrokerSessionError, SessionPhase, SessionSnapshot};
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;

// The peer runs outside Tokio. Keep a blocking task alive so a paused clock
// cannot auto-advance to the deadline before the peer gets CPU time.
pub(crate) async fn drive_io<T>(future: impl Future<Output = T>) -> T {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let mut watchdog =
        tokio::task::spawn_blocking(move || wait.recv_timeout(Duration::from_secs(3)));
    let value = tokio::select! {
        biased;
        value = future => value,
        result = &mut watchdog => panic!("peer did not reach the requested stage: {result:?}"),
    };
    drop(release);
    let _ = watchdog.await.unwrap();
    value
}

pub(crate) async fn advance_with_queued_frame(
    duration: Duration,
    release: std::sync::mpsc::Sender<()>,
    queued: std::sync::mpsc::Receiver<()>,
) {
    let advance = tokio::time::advance(duration);
    tokio::pin!(advance);
    let mut release = Some(release);
    std::future::poll_fn(|context| {
        let result = advance.as_mut().poll(context);
        if let Some(release) = release.take() {
            // advance moves the clock on its first poll, then yields. Wait
            // here so the owner resumes with both the timer and frame ready.
            release.send(()).unwrap();
            queued.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        result
    })
    .await;
}

pub(crate) async fn snapshot(
    state: &mut watch::Receiver<SessionSnapshot>,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    drive_io(async {
        loop {
            let snapshot = state.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            state
                .changed()
                .await
                .expect("driver stopped before publishing its result");
        }
    })
    .await
}

pub(crate) fn assert_expired(snapshot: &SessionSnapshot) {
    assert_eq!(snapshot.phase(), SessionPhase::Failed);
    assert!(matches!(
        snapshot.error(),
        Some(BrokerSessionError::Deadline)
    ));
}
