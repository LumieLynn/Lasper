use lasper_guest_protocol::{
    GuestOperation as Op, WireReply as Reply, WireSessionError as Error, WireSessionPhase as Phase,
    WireSessionState as State,
};

fn preparing() -> State {
    let mut state = State::after_hello(1);
    state.request(2, Op::Prepare).unwrap();
    state
}

fn ready() -> State {
    let mut state = preparing();
    state.reply(2, Op::Prepare, Reply::Ready).unwrap();
    state
}

fn starting() -> State {
    let mut state = ready();
    state.request(3, Op::Start).unwrap();
    state
}

fn fixtures() -> Vec<(State, u64, &'static [Op])> {
    let established = State::after_hello(1);
    let prepare = preparing();
    let mut rejected = prepare;
    rejected.reply(2, Op::Prepare, Reply::Rejected).unwrap();
    let ready = ready();
    let mut pending = ready;
    pending.request(3, Op::Recheck).unwrap();
    let start = starting();
    let mut start_rejected = start;
    start_rejected.reply(3, Op::Start, Reply::Rejected).unwrap();
    let mut accepted = start;
    accepted.reply(3, Op::Start, Reply::StartAccepted).unwrap();
    let mut closing = pending;
    closing.request(4, Op::Cancel).unwrap();
    let mut exited = start;
    exited.exit(2, false).unwrap();
    let mut closed = exited;
    closed.disconnect();
    vec![
        (established, 2, &[Op::Prepare]),
        (prepare, 3, &[]),
        (rejected, 3, &[]),
        (
            ready,
            3,
            &[
                Op::KeepPrepared,
                Op::Recheck,
                Op::Start,
                Op::Cancel,
                Op::Close,
            ],
        ),
        (pending, 4, &[Op::Cancel, Op::Close]),
        (start, 4, &[Op::Close]),
        (start_rejected, 4, &[Op::Close]),
        (accepted, 4, &[Op::Close]),
        (closing, 5, &[]),
        (exited, 4, &[]),
        (closed, 4, &[]),
    ]
}

#[test]
fn every_phase_has_one_request_admission_table() {
    for (state, id, allowed) in fixtures() {
        for operation in [
            Op::Prepare,
            Op::KeepPrepared,
            Op::Recheck,
            Op::Start,
            Op::Cancel,
            Op::Close,
        ] {
            let mut next = state;
            assert_eq!(
                next.request(id, operation).is_ok(),
                allowed.contains(&operation),
                "{:?}: {operation:?}",
                state.phase()
            );
            if !allowed.contains(&operation) {
                assert_eq!(next.phase(), state.phase());
                assert_eq!(next.request(id, operation), Err(Error::InvalidRequestId));
            }
        }
    }
}

#[test]
fn requests_and_replies_must_match_once() {
    let mut state = preparing();
    for (id, op, reply) in [
        (3, Op::Prepare, Reply::Ready),
        (2, Op::Start, Reply::StartAccepted),
        (2, Op::Prepare, Reply::Prepared),
    ] {
        let before = state;
        assert_eq!(state.reply(id, op, reply), Err(Error::UnexpectedReply));
        assert_eq!(state, before);
    }
    state.reply(2, Op::Prepare, Reply::Ready).unwrap();
    assert_eq!(
        state.reply(2, Op::Prepare, Reply::Ready),
        Err(Error::UnexpectedReply)
    );
    assert_eq!(state.request(2, Op::Start), Err(Error::InvalidRequestId));
}

#[test]
fn keep_and_recheck_responses_return_to_ready_including_rejections() {
    for op in [Op::KeepPrepared, Op::Recheck] {
        for reply in [Reply::Prepared, Reply::Rejected] {
            let mut state = ready();
            state.request(3, op).unwrap();
            state.reply(3, op, reply).unwrap();
            assert_eq!(state.phase(), Phase::ReadyReceived);
        }
    }
}

#[test]
fn rejected_cleanup_restores_the_previous_phase_and_consumes_the_request_id() {
    for (state, id, allowed) in fixtures() {
        for op in [Op::Cancel, Op::Close] {
            if !allowed.contains(&op) {
                continue;
            }
            let mut next = state;
            next.request(id, op).unwrap();
            next.reply(id, op, Reply::Rejected).unwrap();
            assert_eq!(next.phase(), state.phase());
            assert_eq!(next.request(id, op), Err(Error::InvalidRequestId));
            next.request(id + 1, op).unwrap();
            next.reply(id + 1, op, Reply::CleanupRequested).unwrap();
            assert_eq!(next.phase(), Phase::Closing);
        }
    }
}

#[test]
fn rejected_cleanup_preserves_inflight_replies_in_either_order() {
    for (op, response, phase) in [
        (Op::KeepPrepared, Reply::Prepared, Phase::ReadyReceived),
        (Op::Recheck, Reply::Rejected, Phase::ReadyReceived),
        (
            Op::Start,
            Reply::StartAccepted,
            Phase::StartAcceptedReceived,
        ),
        (Op::Start, Reply::Rejected, Phase::StartRejected),
    ] {
        for cleanup_first in [false, true] {
            let mut state = ready();
            state.request(3, op).unwrap();
            state.request(4, Op::Close).unwrap();
            let replies = [(3, op, response), (4, Op::Close, Reply::Rejected)];
            for index in if cleanup_first { [1, 0] } else { [0, 1] } {
                let (id, op, reply) = replies[index];
                state.reply(id, op, reply).unwrap();
            }
            assert_eq!(state.phase(), phase);
            state.request(5, Op::Close).unwrap();
        }
    }
}

#[test]
fn rejected_cleanup_cannot_undo_an_exit() {
    let mut state = starting();
    state.request(4, Op::Close).unwrap();
    state.exit(2, true).unwrap();
    state.reply(4, Op::Close, Reply::Rejected).unwrap();
    state.reply(3, Op::Start, Reply::StartAccepted).unwrap();
    assert_eq!(state.phase(), Phase::ExitReceived);
    assert!(state.request(5, Op::Close).is_err());
}

#[test]
fn cleanup_and_inflight_responses_can_arrive_in_either_order() {
    for (op, response) in [
        (Op::KeepPrepared, Reply::Prepared),
        (Op::Recheck, Reply::Rejected),
        (Op::Start, Reply::StartAccepted),
        (Op::Start, Reply::Rejected),
    ] {
        for cleanup_first in [false, true] {
            let mut state = ready();
            state.request(3, op).unwrap();
            state.request(4, Op::Close).unwrap();
            let replies = [(3, op, response), (4, Op::Close, Reply::CleanupRequested)];
            let order = if cleanup_first { [1, 0] } else { [0, 1] };
            for index in order {
                let (id, op, reply) = replies[index];
                state.reply(id, op, reply).unwrap();
                assert_eq!(state.phase(), Phase::Closing);
            }
            state.exit(2, true).unwrap();
        }
    }
}

#[test]
fn exit_can_overtake_responses_but_cannot_reopen_the_session() {
    let mut state = starting();
    state.request(4, Op::Close).unwrap();
    state.exit(2, false).unwrap();
    state.reply(3, Op::Start, Reply::StartAccepted).unwrap();
    state.reply(4, Op::Close, Reply::CleanupRequested).unwrap();
    assert_eq!(state.phase(), Phase::ExitReceived);
    assert!(matches!(
        state.request(5, Op::Start),
        Err(Error::InvalidState { .. })
    ));
    state.exit(2, false).unwrap();
    state.exit(2, true).unwrap();
    assert_eq!(state.exit(2, true), Err(Error::UnexpectedReply));
}

#[test]
fn exit_is_bound_to_prepare_and_requires_ready() {
    for (state, _, _) in fixtures() {
        let permitted = matches!(
            state.phase(),
            Phase::ReadyReceived
                | Phase::PreparedReplyPending
                | Phase::StartSent
                | Phase::StartRejected
                | Phase::StartAcceptedReceived
                | Phase::Closing
                | Phase::ExitReceived
        );
        let mut next = state;
        assert_eq!(
            next.exit(2, false).is_ok(),
            permitted,
            "{:?}",
            state.phase()
        );
        let mut next = state;
        assert_eq!(next.exit(99, true), Err(Error::UnexpectedReply));
    }
}

#[test]
fn disconnect_ends_every_phase_without_claiming_cleanup() {
    for (mut state, _, _) in fixtures() {
        state.disconnect();
        assert_eq!(state.phase(), Phase::ConnectionClosed);
        assert!(state.request(100, Op::Prepare).is_err());
        assert!(state.reply(2, Op::Prepare, Reply::Ready).is_err());
        assert!(state.exit(2, true).is_err());
    }
}

#[test]
fn exit_during_a_partial_send_keeps_the_reserved_response_without_reopening() {
    for final_report in [false, true] {
        for op in [
            Op::KeepPrepared,
            Op::Recheck,
            Op::Start,
            Op::Cancel,
            Op::Close,
        ] {
            let mut state = ready();
            state.queue_request(3, op).unwrap();
            assert_eq!(state.phase(), Phase::ReadyReceived);
            assert!(state.queue_request(4, Op::Close).is_err());
            state.exit(2, final_report).unwrap();
            state.request_sent().unwrap();
            let reply = match op {
                Op::KeepPrepared | Op::Recheck => Reply::Prepared,
                Op::Start => Reply::StartAccepted,
                Op::Cancel | Op::Close => Reply::CleanupRequested,
                _ => unreachable!(),
            };
            state.reply(3, op, reply).unwrap();
            assert_eq!(state.phase(), Phase::ExitReceived);
            if final_report {
                assert_eq!(state.exit(2, true), Err(Error::UnexpectedReply));
            } else {
                state.exit(2, true).unwrap();
            }
        }
    }
}

#[test]
fn normal_reply_can_arrive_while_cleanup_is_partially_written() {
    let mut state = starting();
    state.queue_request(4, Op::Close).unwrap();
    state.reply(3, Op::Start, Reply::Rejected).unwrap();
    state.request_sent().unwrap();
    state.reply(4, Op::Close, Reply::CleanupRequested).unwrap();
    assert_eq!(state.phase(), Phase::Closing);
}

#[test]
fn an_unsent_request_has_no_matching_response() {
    let mut state = ready();
    state.queue_request(3, Op::Start).unwrap();
    assert_eq!(
        state.reply(3, Op::Start, Reply::StartAccepted),
        Err(Error::UnexpectedReply)
    );
    state.disconnect();
    assert!(state.request_sent().is_err());
}
