use super::*;
use lasper_guest_protocol::{
    CommandArgv, ExecutionKind, GuestProcess, PrepareRequest, PrepareRequestFrame,
    ProjectionRequests, StartRequest, TerminalEnvironment, TerminalSize,
};

const BROKER: BrokerId = BrokerId::new([1; 16]);
const PREPARATION: PreparationId = PreparationId::new([2; 16]);

fn request() -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        10,
        BROKER,
        PrepareRequest::new(
            "alice",
            ExecutionKind::Interactive,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/editor"]).unwrap()),
            Some(TerminalEnvironment::new("xterm", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}

fn awaiting_start() -> SessionLifecycle {
    let mut lifecycle = SessionLifecycle::new(BROKER, PREPARATION, 100, 10_000).unwrap();
    lifecycle.begin_ready_delivery().unwrap();
    lifecycle.ready_delivered().unwrap();
    lifecycle
}

#[test]
fn setup_deadline_is_separate_from_the_prepared_lease() {
    let mut session = SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
    assert_eq!(session.phase(), SessionPhase::Preparing);
    assert_eq!(session.deadline_ms(), 5_100);
    assert_eq!(
        session.control(101, control(PreparationControlKind::KeepPrepared)),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Preparing
        })
    );
    assert_eq!(
        session.worker_prepared(5_099),
        Ok(SessionAction::Prepared {
            deadline_ms: 35_099
        })
    );
    assert_eq!(session.phase(), SessionPhase::Prepared);
    assert_eq!(session.tick(5_100), None);
    session.begin_ready_delivery().unwrap();
    assert_eq!(session.deadline_ms(), 35_099);
    assert_eq!(session.tick(5_101), None);
    session.ready_delivered().unwrap();
    assert_eq!(session.tick(35_098), None);

    let mut expired = SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
    assert_eq!(
        expired.worker_prepared(5_100),
        Ok(SessionAction::BeginCleanup(
            CleanupReason::PreparationExpired
        ))
    );
    assert_eq!(expired.phase(), SessionPhase::Closing);
}

#[test]
fn client_and_broker_loss_cancel_both_preparing_and_running_workers() {
    for connection in [SessionConnection::Client, SessionConnection::Broker] {
        let reason = match connection {
            SessionConnection::Client => CleanupReason::ClientClosed,
            SessionConnection::Broker => CleanupReason::BrokerExited,
            SessionConnection::Supervisor => CleanupReason::SupervisorStopped,
        };
        let mut preparing = SessionLifecycle::preparing(BROKER, PREPARATION, 100, 30_000).unwrap();
        assert_eq!(
            preparing.connection_closed(connection),
            SessionAction::BeginCleanup(reason)
        );
        assert_eq!(preparing.connection_closed(connection), SessionAction::Noop);
        let mut running = awaiting_start();
        running.accept_start(101, start_frame()).unwrap();
        running.exec_outcome(ExecOutcome::Started).unwrap();
        assert_eq!(
            running.connection_closed(connection),
            SessionAction::BeginCleanup(reason)
        );
    }
}

fn start_frame() -> StartFrame {
    let request = request();
    let start = StartRequest::new(
        &request,
        PREPARATION,
        None,
        None,
        Some(TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap();
    StartFrame::new(11, start)
}

fn control(kind: PreparationControlKind) -> PreparationControlFrame {
    PreparationControlFrame::new(
        12,
        kind,
        lasper_guest_protocol::PreparationControl::new(BROKER, PREPARATION),
    )
}

#[test]
fn start_is_one_shot_and_ends_the_preparation_lease() {
    let mut session = awaiting_start();
    assert_eq!(session.phase(), SessionPhase::AwaitingStart);

    let frame = start_frame();
    let expected = frame.request().clone();
    let decision = session.accept_start(101, frame.clone()).unwrap();
    assert_eq!(decision, StartDecision::Accepted(expected));
    assert_eq!(session.phase(), SessionPhase::Starting);
    assert_eq!(session.tick(u64::MAX), None);
    assert_eq!(
        session.accept_start(102, frame),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Starting,
        }),
    );
}

#[test]
fn start_requires_completed_ready_delivery() {
    let mut session = SessionLifecycle::new(BROKER, PREPARATION, 100, 10_000).unwrap();
    assert_eq!(
        session.accept_start(101, start_frame()),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Prepared
        }),
    );
    assert_eq!(session.phase(), SessionPhase::Prepared);
    assert_eq!(
        session.control(101, control(PreparationControlKind::KeepPrepared)),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Prepared
        }),
    );
    session.begin_ready_delivery().unwrap();
    assert_eq!(session.phase(), SessionPhase::DeliveringReady);
    session.ready_delivered().unwrap();
    assert_eq!(session.phase(), SessionPhase::AwaitingStart);
    assert!(matches!(
        session.accept_start(102, start_frame()),
        Ok(StartDecision::Accepted(_))
    ));
}

#[test]
fn expiry_wins_at_the_deadline_before_a_queued_start() {
    let mut session = awaiting_start();
    assert_eq!(session.deadline_ms(), 10_100);
    assert_eq!(
        session.accept_start(10_100, start_frame()),
        Ok(StartDecision::BeginCleanup(CleanupReason::LeaseExpired)),
    );
    assert_eq!(session.phase(), SessionPhase::Closing);
}

#[test]
fn serialized_start_and_cancel_have_one_winner() {
    let mut start_wins = awaiting_start();
    let frame = start_frame();
    start_wins.accept_start(101, frame).unwrap();
    assert_eq!(
        start_wins.control(102, control(PreparationControlKind::Cancel)),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Starting,
        }),
    );

    let mut cancel_wins = awaiting_start();
    assert_eq!(
        cancel_wins.control(101, control(PreparationControlKind::Cancel)),
        Ok(SessionAction::BeginCleanup(CleanupReason::Cancelled)),
    );
    assert_eq!(
        cancel_wins.accept_start(102, start_frame()),
        Err(SessionLifecycleError::InvalidTransition {
            phase: SessionPhase::Closing,
        }),
    );
}

#[test]
fn keep_prepared_renews_only_a_live_preparation() {
    let mut session = awaiting_start();
    assert_eq!(
        session.control(1_000, control(PreparationControlKind::KeepPrepared)),
        Ok(SessionAction::LeaseRenewed {
            deadline_ms: 46_000,
        }),
    );
    assert_eq!(session.deadline_ms(), 46_000);
    assert_eq!(
        session.control(46_000, control(PreparationControlKind::KeepPrepared)),
        Ok(SessionAction::BeginCleanup(CleanupReason::LeaseExpired)),
    );
}

#[test]
fn start_result_preserves_failed_and_unknown_outcomes() {
    let mut failed = awaiting_start();
    let frame = start_frame();
    failed.accept_start(101, frame).unwrap();
    assert_eq!(
        failed.exec_outcome(ExecOutcome::Failed { errno: 2 }),
        Ok(SessionAction::BeginCleanup(CleanupReason::ExecFailed {
            errno: 2
        })),
    );

    let mut unknown = awaiting_start();
    let frame = start_frame();
    unknown.accept_start(101, frame).unwrap();
    assert_eq!(
        unknown.exec_outcome(ExecOutcome::Unknown),
        Ok(SessionAction::BeginCleanup(
            CleanupReason::StartOutcomeUnknown
        )),
    );
}

#[test]
fn worker_exit_does_not_require_a_running_observation() {
    let mut session = awaiting_start();
    session.accept_start(101, start_frame()).unwrap();
    assert_eq!(
        session.worker_exited(),
        SessionAction::BeginCleanup(CleanupReason::WorkerExited)
    );
    assert_eq!(
        session.exec_outcome(ExecOutcome::Unknown),
        Ok(SessionAction::Noop)
    );
    assert_eq!(
        session.cleanup_result(true),
        Ok(SessionAction::Closed(CleanupReason::WorkerExited))
    );
}

#[test]
fn close_and_cleanup_pending_keep_one_cleanup_owner() {
    let mut session = awaiting_start();
    let frame = start_frame();
    session.accept_start(101, frame).unwrap();
    session.exec_outcome(ExecOutcome::Started).unwrap();
    assert_eq!(
        session.control(102, control(PreparationControlKind::Close)),
        Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
    );
    assert_eq!(
        session.cleanup_result(false),
        Ok(SessionAction::CleanupPending(CleanupReason::ClientClosed)),
    );
    assert_eq!(session.phase(), SessionPhase::CleanupPending);
    assert_eq!(
        session.retry_cleanup(),
        Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
    );
    assert_eq!(
        session.cleanup_result(true),
        Ok(SessionAction::Closed(CleanupReason::ClientClosed)),
    );
    assert_eq!(session.phase(), SessionPhase::Closed);
    assert_eq!(
        session.control(200, control(PreparationControlKind::Close)),
        Ok(SessionAction::Noop),
    );
}

#[test]
fn close_cancels_an_unstarted_preparation() {
    let mut session = awaiting_start();
    assert_eq!(
        session.control(101, control(PreparationControlKind::Close)),
        Ok(SessionAction::BeginCleanup(CleanupReason::ClientClosed)),
    );
    assert_eq!(session.phase(), SessionPhase::Closing);
}

#[test]
fn commands_must_match_the_preparation_owner() {
    let mut session = awaiting_start();
    let wrong = PreparationControlFrame::new(
        12,
        PreparationControlKind::Cancel,
        lasper_guest_protocol::PreparationControl::new(BROKER, PreparationId::new([3; 16])),
    );
    assert_eq!(
        session.control(101, wrong),
        Err(SessionLifecycleError::PreparationMismatch),
    );
    assert_eq!(session.phase(), SessionPhase::AwaitingStart);
}

#[test]
fn prepared_session_rejects_a_zero_or_overlong_lease() {
    assert_eq!(
        SessionLifecycle::new(BROKER, PREPARATION, 0, 0),
        Err(SessionLifecycleError::InvalidLease),
    );
    assert_eq!(
        SessionLifecycle::new(BROKER, PREPARATION, 0, MAX_PREPARED_LEASE_MS + 1),
        Err(SessionLifecycleError::InvalidLease),
    );
}
