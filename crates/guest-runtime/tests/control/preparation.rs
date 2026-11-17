extern crate std;

use super::*;
use lasper_guest_protocol::*;
use std::os::fd::AsRawFd;
use std::time::Duration;

use super::test_fixture as fixture;
use fixture::wire;

fn blocked_worker(_: WorkerBootstrap<'_>) -> ! {
    loop {
        let _ = linux::poll(&mut [], Some(10));
    }
}

fn setup_timeout(context: GuardianContext) -> i32 {
    if run_with_worker(&context, blocked_worker).is_ok() {
        0
    } else {
        71
    }
}

fn exited_worker(_: WorkerBootstrap<'_>) -> ! {
    linux::exit(23)
}

fn exited_before_ready(context: GuardianContext) -> i32 {
    let run = || -> Result<(), GuardianError> {
        let mut wire =
            lasper_guest_protocol::WireSessionState::after_hello(context.hello_request_id());
        let storage = prepare::receive(&context, &mut wire)?;
        let request = storage
            .request_with(context.negotiated_protocol())
            .map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().context("read test preparation clock")?;
        let (preparation, mut guardian) = prepare::assign(&context, &request, now, exited_worker)?;
        // Wait for real exit evidence without reaping, so the first driver
        // observation tests premature exit rather than a setup pipe failure.
        let mut pidfd = [linux::PollFd::new(guardian.poll_descriptors()[1])];
        linux::poll(&mut pidfd, Some(1_000)).context("wait for test worker exit")?;
        if !pidfd[0].ready() {
            return Err(GuardianError::NotPrepared);
        }
        driver::run(&context, &request, preparation, &mut guardian, wire)
    };
    if run().is_ok() {
        0
    } else {
        72
    }
}

fn expect_rejection(entry: fn(GuardianContext) -> i32, code: GuestErrorCode) {
    expect_rejection_with_limits(entry, code, ExecutionKind::Piped, ControlLimits::CURRENT);
}

fn expect_rejection_with_limits(
    entry: fn(GuardianContext) -> i32,
    code: GuestErrorCode,
    execution: ExecutionKind,
    limits: ControlLimits,
) {
    let mut guardian = fixture::ForkedGuardian::spawn_with_limits(entry, limits);
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let request = PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            execution,
            GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            (execution == ExecutionKind::Terminal).then(|| TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    );
    let mut bytes = [0; 2048];
    let length = fixture::protocol()
        .encode_prepare_frame(&request, &mut bytes)
        .unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    wire::send(
        guardian.control.as_raw_fd(),
        header,
        &bytes[CONTROL_FRAME_HEADER_BYTES..length],
        &[],
    )
    .unwrap();
    let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = fixture::protocol()
        .decode_error(
            header,
            &request,
            None,
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    assert_eq!(error.code(), code);
    assert_eq!(error.errno(), None);
    guardian.wait();
}

#[test]
fn immediate_prepare_with_stalled_worker_setup_returns_expired() {
    expect_rejection(setup_timeout, GuestErrorCode::Expired);
}

#[test]
fn worker_exit_before_ready_has_its_own_rejection_code() {
    expect_rejection(exited_before_ready, GuestErrorCode::WorkerExitedBeforeStart);
}

fn insufficient_ready_capacity(context: GuardianContext) -> i32 {
    match run_with_worker(&context, exited_worker) {
        Err(GuardianError::ReadyDescriptorCapacity {
            required,
            available,
        }) if required > available => {}
        _ => return 73,
    }
    let mut child_exists = false;
    if linux::children::visit_direct_children(linux::pid(), linux::tid(), |_| {
        child_exists = true;
        Ok(())
    })
    .is_err()
        || child_exists
    {
        return 74;
    }
    0
}

#[test]
fn ready_capacity_is_rejected_before_creating_a_worker() {
    for execution in [ExecutionKind::Terminal, ExecutionKind::Piped] {
        let required = PrepareReady::descriptor_count_for(execution);
        for available in [0, 2, 3, 4]
            .into_iter()
            .filter(|available| *available < required)
        {
            expect_rejection_with_limits(
                insufficient_ready_capacity,
                GuestErrorCode::InvalidRequest,
                execution,
                ControlLimits::new(ControlLimits::CURRENT.max_control_frame_bytes(), available)
                    .unwrap(),
            );
        }
    }
}

fn prepared_worker(context: GuardianContext) -> i32 {
    run_with_worker(&context, super::super::worker::clock_fixture::prepared).map_or(75, |()| 0)
}

#[test]
fn sufficient_descriptor_limits_deliver_ready_and_allow_cleanup() {
    for (execution, available) in [
        (ExecutionKind::Terminal, 3),
        (ExecutionKind::Terminal, 4),
        (ExecutionKind::Piped, 4),
    ] {
        let limits =
            ControlLimits::new(ControlLimits::CURRENT.max_control_frame_bytes(), available)
                .unwrap();
        let mut guardian = fixture::ForkedGuardian::spawn_with_limits(prepared_worker, limits);
        let protocol = negotiate_hello(
            HelloRequest::new(FeatureSet::WORKER_PIDFD, limits).unwrap(),
            HelloOffer::local(ControlLimits::CURRENT),
        )
        .unwrap();
        let request = PrepareRequestFrame::new(
            10,
            BrokerId::new([1; 16]),
            PrepareRequest::new(
                "root",
                execution,
                GuestProcess::LoginShell,
                ProcessEnvironment::default(),
                (execution == ExecutionKind::Terminal).then(|| TerminalSize::new(80, 24).unwrap()),
            )
            .unwrap(),
        );
        let mut bytes = [0; 2048];
        let length = protocol.encode_prepare_frame(&request, &mut bytes).unwrap();
        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
        wire::send(
            guardian.control.as_raw_fd(),
            header,
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            &[],
        )
        .unwrap();
        let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
        let ready = protocol
            .decode_ready(
                &request,
                frame.header,
                &frame.payload,
                frame.descriptors.len(),
            )
            .unwrap();
        assert_eq!(
            frame.descriptors.len(),
            PrepareReady::descriptor_count_for(execution) as usize
        );
        let cancel = PreparationControlFrame::new(
            11,
            PreparationControlKind::Cancel,
            PreparationControl::new(request.broker_id(), ready.response().preparation_id()),
        );
        let (header, length) = protocol
            .encode_control_request(&cancel, &mut bytes)
            .unwrap();
        wire::send(guardian.control.as_raw_fd(), header, &bytes[..length], &[]).unwrap();
        let response = wire::receive(guardian.control.as_raw_fd()).unwrap();
        let reply = protocol
            .decode_control_reply(
                &cancel,
                response.header,
                &response.payload,
                response.descriptors.len(),
            )
            .unwrap();
        assert_eq!(reply.status(), ControlReplyStatus::CleanupRequested);
        loop {
            let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
            let exit = protocol
                .decode_exit(
                    &request,
                    ready.response().preparation_id(),
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            assert_eq!(exit.report().reason(), ExitReason::Cancelled);
            assert_eq!(exit.report().execution(), ExecutionStatus::NotStarted);
            if exit.report().is_final() {
                break;
            }
        }
        guardian.wait();
    }
}

#[test]
fn terminal_with_three_descriptors_reaches_worker_preparation() {
    expect_rejection_with_limits(
        exited_before_ready,
        GuestErrorCode::WorkerExitedBeforeStart,
        ExecutionKind::Terminal,
        ControlLimits::new(ControlLimits::CURRENT.max_control_frame_bytes(), 3).unwrap(),
    );
}
