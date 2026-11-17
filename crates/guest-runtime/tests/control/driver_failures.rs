extern crate std;

use super::super::{prepare, test_fixture as fixture};
use super::*;
use crate::session::{worker::clock_fixture, WorkerBootstrap, WorkerExit};
use lasper_guest_protocol::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use fixture::wire;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Scenario {
    BeforeReady,
    AfterReady,
    QueuedReply,
    PartialInput,
    Backpressure,
    Poisoned,
    ObservationFailure,
}

fn blocked_worker(_: WorkerBootstrap<'_>) -> ! {
    loop {
        let _ = linux::poll(&mut [], Some(10));
    }
}

fn request() -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    )
}

fn run(context: GuardianContext, scenario: Scenario) -> i32 {
    let run = || -> Result<(), GuardianError> {
        let mut wire =
            lasper_guest_protocol::WireSessionState::after_hello(context.hello_request_id());
        let storage = prepare::receive(&context, &mut wire)?;
        let request = storage
            .request_with(context.negotiated_protocol())
            .map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().context("read test preparation clock")?;
        let entry = match scenario {
            Scenario::BeforeReady => blocked_worker,
            _ => clock_fixture::prepared,
        };
        let (preparation, mut guardian) = prepare::assign(&context, &request, now, entry)?;
        if scenario != Scenario::BeforeReady {
            loop {
                let now = linux::monotonic_ms().context("read test Ready clock")?;
                let observation = guardian.poll(now)?;
                if matches!(
                    observation.phase,
                    SessionPhase::Prepared | SessionPhase::DeliveringReady
                ) && guardian
                    .deliver_ready(now, &mut IoBudget::new())?
                    .is_ready()
                {
                    wire.reply(
                        request.request_id(),
                        GuestOperation::Prepare,
                        lasper_guest_protocol::WireReply::Ready,
                    )
                    .map_err(|_| GuardianError::InvalidStatus)?;
                    break;
                }
                linux::poll(&mut [], Some(1)).context("wait for test Ready")?;
            }
        }
        let now = linux::monotonic_ms().context("read test session clock")?;
        let mut driver = ControlLoop {
            context: &context,
            request: &request,
            preparation,
            input: None,
            wire,
            sending_reply: None,
            reported: None,
            prepare_rejected: false,
            failure: None,
        };
        prepare_failure(&mut driver, &mut guardian, now, scenario)?;
        let result = if scenario == Scenario::ObservationFailure {
            deny_waitid()?;
            driver.drive(&mut guardian, linux::monotonic_ms)
        } else {
            driver.drive(&mut guardian, || Err(libc::EIO))
        };
        let observation = guardian.poll(now)?;
        let (expected_result, reason, signal) = if scenario == Scenario::ObservationFailure {
            (
                result.is_ok(),
                CleanupReason::WorkerObservationFailed,
                libc::SIGTERM,
            )
        } else {
            (
                matches!(
                    result,
                    Err(GuardianError::System {
                        errno: libc::EIO,
                        ..
                    })
                ),
                CleanupReason::ClockFailed,
                libc::SIGKILL,
            )
        };
        if expected_result
            && observation.phase == SessionPhase::Closed
            && observation.cleanup_reason == Some(reason)
            && observation.worker.exit == Some(WorkerExit::Signaled(signal))
            && guardian.next_deadline_ms().is_none()
        {
            Ok(())
        } else {
            Err(GuardianError::InvalidStatus)
        }
    };
    if run().is_ok() {
        0
    } else {
        72
    }
}

fn deny_waitid() -> Result<(), GuardianError> {
    let filter = [
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: libc::SYS_waitid as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::EIO as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // Only this guardian is restricted, after the worker has reached Ready.
    // wait4 remains available so the test also checks that cleanup still reaps.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0
    {
        return Err(GuardianError::InvalidStatus);
    }
    Ok(())
}

fn prepare_failure(
    driver: &mut ControlLoop<'_, '_>,
    guardian: &mut SessionGuardian<'_, '_>,
    now: u64,
    scenario: Scenario,
) -> Result<(), GuardianError> {
    let connection = driver.context.connection();
    if matches!(scenario, Scenario::QueuedReply | Scenario::Backpressure) {
        queue_recheck_reply(driver, guardian, now, scenario == Scenario::Backpressure)?;
    }
    if scenario == Scenario::PartialInput {
        let mut reader = Input::new(connection)?;
        loop {
            reader = match reader.advance(now, &mut IoBudget::new())? {
                InputProgress::Pending(next) => next,
                InputProgress::Complete(_) => return Err(GuardianError::InvalidStatus),
            };
            if reader.deadline_ms().is_some() {
                break;
            }
            linux::poll(&mut [], Some(1)).context("wait for partial test command")?;
        }
        driver.input = Some(reader);
    }
    if scenario == Scenario::Poisoned {
        connection.poison();
    }
    Ok(())
}

fn queue_recheck_reply(
    driver: &mut ControlLoop<'_, '_>,
    guardian: &mut SessionGuardian<'_, '_>,
    now: u64,
    backpressure: bool,
) -> Result<(), GuardianError> {
    let mut reader = Input::new(driver.context.connection())?;
    let frame = loop {
        match reader.advance(now, &mut IoBudget::new())? {
            InputProgress::Complete(frame) => break frame,
            InputProgress::Pending(next) => reader = next,
        }
        linux::poll(&mut [], Some(1)).context("wait for test command")?;
    };
    let command = fixture::protocol()
        .decode_control_request(
            11,
            driver.request,
            driver.preparation,
            frame.header(),
            frame.payload(),
            0,
        )
        .map_err(GuardianError::SessionFrame)?;
    driver
        .wire
        .request(11, GuestOperation::Recheck)
        .map_err(|_| GuardianError::InvalidStatus)?;
    driver.sending_reply = Some((
        11,
        GuestOperation::Recheck,
        lasper_guest_protocol::WireReply::Prepared,
    ));
    guardian.control(now, command)?;
    let reply = ControlReplyFrame::new(
        command,
        ControlReplyStatus::Prepared {
            remaining_lease_ms: guardian.remaining_lease_ms(now),
        },
    )
    .map_err(|_| GuardianError::ReplyEncoding)?;
    let capacity = if backpressure {
        let bytes: i32 = 4096;
        // SAFETY: setsockopt borrows this live integer for the call.
        if unsafe {
            libc::setsockopt(
                driver.context.connection().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&bytes as *const i32).cast(),
                core::mem::size_of::<i32>() as _,
            )
        } != 0
        {
            return Err(GuardianError::InvalidStatus);
        }
        64 * 1024
    } else {
        4096
    };
    let payload = Payload::encode(capacity, |bytes| {
        let (header, length) = fixture::protocol()
            .encode_control_reply(&reply, bytes)
            .map_err(GuardianError::SessionFrame)?;
        if !backpressure {
            return Ok((header, length));
        }
        // JSON whitespace keeps the response valid while making this one
        // queued frame larger than the socket's send buffer.
        bytes[length..].fill(b' ');
        let header = ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
            header.kind(),
            header.request_id(),
            capacity as u32,
            0,
        )
        .map_err(|_| GuardianError::ReplyEncoding)?;
        Ok((header, capacity))
    })?;
    guardian.queue_reply(ReplyKind::Command, now, payload)
}

fn spawn(entry: fn(GuardianContext) -> i32) -> fixture::ForkedGuardian {
    let guardian = fixture::ForkedGuardian::spawn(entry);
    let request = request();
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
    guardian
}

fn expect_exit(entry: fn(GuardianContext) -> i32, scenario: Scenario) {
    let mut guardian = spawn(entry);
    let request = request();
    let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let ready = fixture::protocol()
        .decode_ready(
            &request,
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    let command = PreparationControlFrame::new(
        11,
        PreparationControlKind::Recheck,
        PreparationControl::new(request.broker_id(), ready.response().preparation_id()),
    );
    if matches!(scenario, Scenario::QueuedReply | Scenario::Backpressure) {
        let mut bytes = [0; 1024];
        let (header, length) = fixture::protocol()
            .encode_control_request(&command, &mut bytes)
            .unwrap();
        wire::send(guardian.control.as_raw_fd(), header, &bytes[..length], &[]).unwrap();
        if scenario == Scenario::Backpressure {
            guardian.wait();
            let mut partial = std::vec::Vec::new();
            guardian.control.read_to_end(&mut partial).unwrap();
            assert!(!partial.is_empty());
            assert!(partial.len() < CONTROL_FRAME_HEADER_BYTES + 64 * 1024);
            return;
        }
        let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
        fixture::protocol()
            .decode_control_reply(&command, frame.header, &frame.payload, 0)
            .unwrap();
    }
    if scenario == Scenario::PartialInput {
        let (header, _) = fixture::protocol()
            .encode_control_request(&command, &mut [0; 1024])
            .unwrap();
        guardian.control.write_all(&header.encode()[..1]).unwrap();
    }
    if scenario != Scenario::Poisoned {
        loop {
            let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
            let exit = fixture::protocol()
                .decode_exit(
                    &request,
                    ready.response().preparation_id(),
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            assert_eq!(
                exit.report().reason(),
                if scenario == Scenario::ObservationFailure {
                    ExitReason::WorkerObservationFailed
                } else {
                    ExitReason::ClockFailed
                }
            );
            if exit.report().is_final() {
                assert_eq!(
                    exit.report().exit(),
                    Some(ExitStatus::Signaled(
                        if scenario == Scenario::ObservationFailure {
                            libc::SIGTERM as u8
                        } else {
                            libc::SIGKILL as u8
                        }
                    ))
                );
                break;
            }
        }
    }
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
    guardian.wait();
}

#[test]
fn worker_observation_failure_after_ready_keeps_its_reason_and_reaps() {
    expect_exit(
        |context| run(context, Scenario::ObservationFailure),
        Scenario::ObservationFailure,
    );
}

#[test]
fn clock_failure_before_ready_sends_a_correlated_system_error_and_reaps() {
    let mut guardian = spawn(|context| run(context, Scenario::BeforeReady));
    let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let request = request();
    let mut bytes = [0; 2048];
    fixture::protocol()
        .encode_prepare_frame(&request, &mut bytes)
        .unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    let error = fixture::protocol()
        .decode_error(header, &request, None, frame.header, &frame.payload, 0)
        .unwrap();
    assert_eq!(error.code(), GuestErrorCode::System);
    assert_eq!(error.errno(), Some(libc::EIO));
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
    guardian.wait();
}

#[test]
fn clock_failure_after_ready_reaches_the_host_as_exit() {
    expect_exit(
        |context| run(context, Scenario::AfterReady),
        Scenario::AfterReady,
    );
}

#[test]
fn clock_failure_finishes_an_already_queued_reply_before_exit() {
    expect_exit(
        |context| run(context, Scenario::QueuedReply),
        Scenario::QueuedReply,
    );
}

#[test]
fn clock_failure_preserves_exit_delivery_with_a_partial_inbound_frame() {
    expect_exit(
        |context| run(context, Scenario::PartialInput),
        Scenario::PartialInput,
    );
}

#[test]
fn clock_failure_does_not_wait_for_a_backpressured_host_to_read() {
    expect_exit(
        |context| run(context, Scenario::Backpressure),
        Scenario::Backpressure,
    );
}

#[test]
fn clock_failure_keeps_reaping_when_the_connection_is_already_poisoned() {
    expect_exit(
        |context| run(context, Scenario::Poisoned),
        Scenario::Poisoned,
    );
}
