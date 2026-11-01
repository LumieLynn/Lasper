use super::*;
use crate::application::sessions::execution::{
    GuestCommand, GuestEnvironment, GuestExecutionObservation,
};
use crate::application::sessions::{
    terminal_session_channel, GuestTarget, TerminalSessionEndpoint, ValidatedGuestUserName,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionLifecycle, TerminalAttachmentKind};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

struct Reply;
impl GuestOutputBoundary for Reply {
    fn complete(&self, output: &[u8]) -> Result<Option<usize>, SessionError> {
        Ok(output
            .windows(5)
            .position(|part| part == b"done\n")
            .map(|offset| offset + 5))
    }
}

fn limits() -> GuestOutputLimits {
    GuestOutputLimits {
        deadline: tokio::time::Instant::now() + Duration::from_secs(3),
        opening: Duration::from_secs(2),
        capture: Duration::from_secs(1),
        bytes: 16,
    }
}

fn session() -> (TerminalSessionHandle, TerminalSessionEndpoint) {
    terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login)
}

#[tokio::test]
async fn reply_boundary_wins_over_later_transport_output_and_closes_session() {
    let (handle, mut endpoint) = session();
    endpoint
        .output
        .send(b"done\ntrailing output longer than limit".to_vec())
        .await
        .unwrap();
    let output = collect_output(handle, &limits(), &Reply).await.unwrap();
    assert_eq!(output.bytes, b"done\n");
    assert_eq!(output.end, GuestOutputEnd::Complete);
    endpoint.close.try_recv().unwrap();
}

#[tokio::test]
async fn transport_exit_does_not_claim_a_successful_guest_command() {
    let (handle, mut endpoint) = session();
    endpoint.output.send(b"partial".to_vec()).await.unwrap();
    endpoint
        .lifecycle
        .send(SessionLifecycle::Exited {
            success: true,
            code: None,
        })
        .unwrap();
    drop(endpoint.output);
    let output = collect_output(handle, &limits(), &Reply).await.unwrap();
    assert_eq!(output.bytes, b"partial");
    assert_eq!(output.end, GuestOutputEnd::Closed);
    endpoint.close.try_recv().unwrap();
}

#[tokio::test(start_paused = true)]
async fn deadline_and_limit_keep_partial_output_and_close_the_owner() {
    let (handle, mut endpoint) = session();
    endpoint.output.send(b"partial".to_vec()).await.unwrap();
    let output = collect_output(handle, &limits(), &Reply).await.unwrap();
    assert_eq!(output.bytes, b"partial");
    assert_eq!(output.end, GuestOutputEnd::TimedOut);
    endpoint.close.try_recv().unwrap();

    let (handle, mut endpoint) = session();
    endpoint.output.send(vec![b'x'; 17]).await.unwrap();
    let output = collect_output(handle, &limits(), &Reply).await.unwrap();
    assert_eq!(output.bytes.len(), 16);
    assert_eq!(output.end, GuestOutputEnd::LimitReached);
    endpoint.close.try_recv().unwrap();
}

#[derive(Clone)]
struct FakeExecution {
    calls: Arc<Mutex<Vec<&'static str>>>,
    session: Arc<Mutex<Option<TerminalSessionHandle>>>,
    wait_open: bool,
    prepare_delay: Duration,
}
struct FakePrepared {
    execution: FakeExecution,
    deadline: tokio::time::Instant,
}

#[async_trait]
impl GuestExecutionPort for FakeExecution {
    type Prepared = FakePrepared;
    async fn observe(
        &self,
        _: &MachineName,
        _: tokio::time::Instant,
    ) -> Result<GuestExecutionObservation, SessionError> {
        unreachable!("preparation owns readiness selection")
    }
    async fn prepare(
        &self,
        _: GuestExecutionRequest,
        deadline: tokio::time::Instant,
    ) -> Result<FakePrepared, GuestExecutionError> {
        self.calls.lock().unwrap().push("prepare");
        tokio::time::timeout_at(deadline, tokio::time::sleep(self.prepare_delay))
            .await
            .map_err(|_| {
                GuestExecutionError::new(
                    GuestExecutionFailure::NotAttempted,
                    SessionError::new("preparation expired"),
                )
            })?;
        Ok(FakePrepared {
            execution: self.clone(),
            deadline,
        })
    }
}

#[async_trait]
impl PreparedGuestExecution for FakePrepared {
    type Session = TerminalSessionHandle;
    async fn open(self) -> Result<TerminalSessionHandle, GuestExecutionError> {
        self.execution.calls.lock().unwrap().push("open");
        if self.execution.wait_open {
            tokio::time::sleep_until(self.deadline).await;
            return Err(GuestExecutionError::new(
                GuestExecutionFailure::OutcomeUnknown,
                SessionError::new("open expired"),
            ));
        }
        Ok(self.execution.session.lock().unwrap().take().unwrap())
    }
    async fn open_local(
        self,
        _: SessionId,
        _: SessionSize,
    ) -> Result<TerminalSessionHandle, GuestExecutionError> {
        self.open().await
    }
}

fn request() -> GuestExecutionRequest {
    GuestExecutionRequest::command(
        GuestTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        ),
        GuestEnvironment::Probe,
        GuestCommand::new("/bin/true", vec![]).unwrap(),
    )
}

#[tokio::test]
async fn bounded_entry_consumes_one_preparation_and_uses_the_common_collector() {
    let (handle, mut endpoint) = session();
    endpoint.output.send(b"done\n".to_vec()).await.unwrap();
    let execution = FakeExecution {
        calls: Arc::default(),
        session: Arc::new(Mutex::new(Some(handle))),
        wait_open: false,
        prepare_delay: Duration::ZERO,
    };
    let output = run_bounded(
        &execution,
        request(),
        SessionId::new(1).unwrap(),
        &limits(),
        &Reply,
    )
    .await
    .unwrap();
    assert_eq!(output.end, GuestOutputEnd::Complete);
    assert_eq!(*execution.calls.lock().unwrap(), ["prepare", "open"]);
    endpoint.close.try_recv().unwrap();
}

#[tokio::test(start_paused = true)]
async fn opening_timeout_is_unknown_and_never_retries_a_submitted_request() {
    let execution = FakeExecution {
        calls: Arc::default(),
        session: Arc::default(),
        wait_open: true,
        prepare_delay: Duration::ZERO,
    };
    let error = run_bounded(
        &execution,
        request(),
        SessionId::new(1).unwrap(),
        &limits(),
        &Reply,
    )
    .await
    .unwrap_err();
    assert_eq!(error.failure, GuestExecutionFailure::OutcomeUnknown);
    assert_eq!(*execution.calls.lock().unwrap(), ["prepare", "open"]);
}

#[tokio::test(start_paused = true)]
async fn preparation_timeout_never_opens_the_guest_process() {
    let execution = FakeExecution {
        calls: Arc::default(),
        session: Arc::default(),
        wait_open: false,
        prepare_delay: Duration::from_secs(4),
    };
    let error = run_bounded(
        &execution,
        request(),
        SessionId::new(1).unwrap(),
        &limits(),
        &Reply,
    )
    .await
    .unwrap_err();
    assert_eq!(error.failure, GuestExecutionFailure::NotAttempted);
    assert_eq!(*execution.calls.lock().unwrap(), ["prepare"]);
}

#[tokio::test(start_paused = true)]
async fn another_candidate_cannot_restart_the_outer_deadline() {
    let (handle, mut endpoint) = session();
    endpoint.output.send(b"partial".to_vec()).await.unwrap();
    let execution = FakeExecution {
        calls: Arc::default(),
        session: Arc::new(Mutex::new(Some(handle))),
        wait_open: false,
        prepare_delay: Duration::ZERO,
    };
    let mut limits = limits();
    limits.deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    limits.capture = Duration::from_millis(40);
    let first = run_bounded(
        &execution,
        request(),
        SessionId::new(1).unwrap(),
        &limits,
        &Reply,
    )
    .await
    .unwrap();
    assert_eq!(first.end, GuestOutputEnd::TimedOut);
    endpoint.close.try_recv().unwrap();
    let error = run_bounded(
        &execution,
        request(),
        SessionId::new(2).unwrap(),
        &limits,
        &Reply,
    )
    .await
    .unwrap_err();
    assert_eq!(error.failure, GuestExecutionFailure::NotAttempted);
    assert_eq!(*execution.calls.lock().unwrap(), ["prepare", "open"]);
}

#[tokio::test]
async fn cancelling_capture_closes_its_session_without_a_cleanup_task() {
    let (handle, mut endpoint) = session();
    let limits = limits();
    let task = tokio::spawn(async move { collect_output(handle, &limits, &Reply).await });
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    endpoint.close.try_recv().unwrap();
}
