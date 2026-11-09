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
        let storage = prepare::receive(&context)?;
        let request = storage.request().map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().context("read test preparation clock")?;
        let (preparation, mut guardian, capture) =
            prepare::assign(&context, &request, now, exited_worker)?;
        // Wait for real exit evidence without reaping, so the first driver
        // observation tests premature exit rather than a setup pipe failure.
        let mut pidfd = [linux::PollFd::new(guardian.poll_descriptors()[1])];
        linux::poll(&mut pidfd, Some(1_000)).context("wait for test worker exit")?;
        if !pidfd[0].ready() {
            return Err(GuardianError::NotPrepared);
        }
        driver::run(&context, &request, preparation, &mut guardian, capture)
    };
    if run().is_ok() {
        0
    } else {
        72
    }
}

fn expect_rejection(entry: fn(GuardianContext) -> i32, code: GuestErrorCode) {
    let mut guardian = fixture::ForkedGuardian::spawn(entry);
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let request = PrepareRequestFrame::new(
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
    );
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
    let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = ErrorFrame::decode_for(
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
