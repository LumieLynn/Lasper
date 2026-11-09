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
    CapturedOutput,
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
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}

fn run(context: GuardianContext, scenario: Scenario) -> i32 {
    let run = || -> Result<(), GuardianError> {
        let storage = prepare::receive(&context)?;
        let request = storage.request().map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().context("read test preparation clock")?;
        let entry = match scenario {
            Scenario::BeforeReady => blocked_worker,
            Scenario::CapturedOutput => clock_fixture::with_output,
            _ => clock_fixture::prepared,
        };
        let (preparation, mut guardian, mut capture) =
            prepare::assign(&context, &request, now, entry)?;
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
            last_request_id: request.request_id(),
            reported: None,
            prepare_rejected: false,
            failure: None,
        };
        prepare_failure(&mut driver, &mut guardian, capture.as_mut(), now, scenario)?;
        let result = driver.drive(&mut guardian, capture, || Err(libc::EIO));
        let observation = guardian.poll(now)?;
        if matches!(
            result,
            Err(GuardianError::System {
                errno: libc::EIO,
                ..
            })
        ) && observation.phase == SessionPhase::Closed
            && observation.cleanup_reason == Some(CleanupReason::ClockFailed)
            && observation.worker.exit == Some(WorkerExit::Signaled(libc::SIGKILL))
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

fn prepare_failure(
    driver: &mut ControlLoop<'_, '_>,
    guardian: &mut SessionGuardian<'_, '_>,
    capture: Option<&mut Capture>,
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
    if scenario == Scenario::CapturedOutput {
        let capture = capture.ok_or(GuardianError::InvalidStatus)?;
        while capture.view()?.stdout().len() < MAX_CAPTURED_OUTPUT_BYTES {
            capture.drain(guardian, now, &mut IoBudget::new())?;
            linux::poll(&mut [], Some(1)).context("wait for captured test output")?;
        }
    }
    if scenario == Scenario::Poisoned {
        connection.poison();
    }
    Ok(())
}

fn queue_recheck_reply(
    driver: &ControlLoop<'_, '_>,
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
    let command = PreparationControlFrame::decode_for(
        11,
        driver.request,
        driver.preparation,
        frame.header(),
        frame.payload(),
        0,
    )
    .map_err(GuardianError::SessionFrame)?;
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
        let (header, length) = reply
            .encode_parts(bytes)
            .map_err(GuardianError::SessionFrame)?;
        if !backpressure {
            return Ok((header, length));
        }
        // JSON whitespace keeps the response valid while making this one
        // queued frame larger than the socket's send buffer.
        bytes[length..].fill(b' ');
        let header =
            ControlFrameHeader::new(header.kind(), header.request_id(), capacity as u32, 0)
                .map_err(|_| GuardianError::ReplyEncoding)?;
        Ok((header, capacity))
    })?;
    guardian.queue_reply(ReplyKind::Command, now, payload)
}

fn spawn(entry: fn(GuardianContext) -> i32) -> fixture::ForkedGuardian {
    let guardian = fixture::ForkedGuardian::spawn(entry);
    let request = request();
    let mut bytes = [0; 2048];
    let length = request.encode_into(&mut bytes).unwrap();
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
    let ready = request
        .decode_ready_parts(frame.header, &frame.payload, frame.descriptors.len())
        .unwrap();
    let command = PreparationControlFrame::new(
        11,
        PreparationControlKind::Recheck,
        PreparationControl::new(request.broker_id(), ready.response().preparation_id()),
    );
    if matches!(scenario, Scenario::QueuedReply | Scenario::Backpressure) {
        let mut bytes = [0; 1024];
        let (header, length) = command.encode_parts(&mut bytes).unwrap();
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
        ControlReplyFrame::decode_for(&command, frame.header, &frame.payload, 0).unwrap();
    }
    if scenario == Scenario::PartialInput {
        let (header, _) = command.encode_parts(&mut [0; 1024]).unwrap();
        guardian.control.write_all(&header.encode()[..1]).unwrap();
    }
    if scenario != Scenario::Poisoned {
        loop {
            let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
            let exit = ExitFrame::decode_for(
                &request,
                ready.response().preparation_id(),
                frame.header,
                &frame.payload,
                frame.descriptors.len(),
            )
            .unwrap();
            assert_eq!(exit.report().reason(), ExitReason::ClockFailed);
            if scenario == Scenario::CapturedOutput {
                let output = exit.output().unwrap();
                assert_eq!(output.stdout().len(), MAX_CAPTURED_OUTPUT_BYTES);
                assert!(output.stdout().truncated());
                assert!(!output.stdout().complete());
            }
            if exit.report().is_final() {
                assert_eq!(
                    exit.report().exit(),
                    Some(ExitStatus::Signaled(libc::SIGKILL as u8))
                );
                break;
            }
        }
    }
    assert_eq!(guardian.control.read(&mut [0]).unwrap(), 0);
    guardian.wait();
}

#[test]
fn clock_failure_before_ready_sends_a_correlated_system_error_and_reaps() {
    let mut guardian = spawn(|context| run(context, Scenario::BeforeReady));
    let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let request = request();
    let mut bytes = [0; 2048];
    request.encode_into(&mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    let error =
        ErrorFrame::decode_for(header, &request, None, frame.header, &frame.payload, 0).unwrap();
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

#[test]
fn clock_failure_reports_captured_output_larger_than_one_io_turn() {
    expect_exit(
        |context| run(context, Scenario::CapturedOutput),
        Scenario::CapturedOutput,
    );
}
