extern crate std;

use super::super::{prepare, test_fixture as fixture};
use super::*;
use crate::session::worker::clock_fixture;
use lasper_guest_protocol::*;
use std::{io::Read, os::fd::AsRawFd, time::Duration};

fn closed_after_observation(context: GuardianContext) -> i32 {
    let run = || -> Result<(), GuardianError> {
        let mut wire = WireSessionState::after_hello(context.hello_request_id());
        let storage = prepare::receive(&context, &mut wire)?;
        let request = storage
            .request_with(context.negotiated_protocol())
            .map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().context("read test preparation clock")?;
        let (preparation, mut guardian) =
            prepare::assign(&context, &request, now, clock_fixture::prepared)?;
        loop {
            let observation = guardian.poll(now)?;
            if observation.phase == SessionPhase::Prepared
                && guardian
                    .deliver_ready(now, &mut IoBudget::new())?
                    .is_ready()
            {
                wire.reply(
                    request.request_id(),
                    GuestOperation::Prepare,
                    WireReply::Ready,
                )
                .map_err(|_| GuardianError::InvalidStatus)?;
                break;
            }
            linux::poll(&mut [], Some(1)).context("wait for test Ready")?;
        }
        let driver = ControlLoop {
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
        let observed_phase = guardian.phase();
        // Reproduce cleanup completing after the iteration's initial poll.
        // No socket input or worker event remains to wake an incorrect wait.
        guardian.force_cleanup(CleanupReason::ClientClosed);
        while guardian.poll(now)?.phase != SessionPhase::Closed {
            linux::poll(&mut [], Some(1)).context("wait for test cleanup")?;
        }
        if guardian.has_reply()
            || guardian.next_deadline_ms().is_some()
            || guardian.poll_descriptors() != [-1, -1]
        {
            return Err(GuardianError::InvalidStatus);
        }
        driver.wait(&guardian, observed_phase, true, now, None)?;
        driver.drive(&mut guardian, linux::monotonic_ms)
    };
    run().map_or(71, |()| 0)
}

#[test]
fn cleanup_completed_after_observation_sends_exit_without_waiting_for_another_event() {
    let mut child = fixture::ForkedGuardian::spawn(closed_after_observation);
    child
        .control
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let request = PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::LoginShell,
            ProcessEnvironment::default(),
            None,
        )
        .unwrap(),
    );
    let protocol = fixture::protocol();
    let mut bytes = [0; 2048];
    let length = protocol.encode_prepare_frame(&request, &mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    fixture::wire::send(
        child.control.as_raw_fd(),
        header,
        &bytes[CONTROL_FRAME_HEADER_BYTES..length],
        &[],
    )
    .unwrap();
    let ready = fixture::wire::receive(child.control.as_raw_fd()).unwrap();
    let ready_frame = protocol
        .decode_ready(
            &request,
            ready.header,
            &ready.payload,
            ready.descriptors.len(),
        )
        .unwrap();
    let exit = fixture::wire::receive(child.control.as_raw_fd()).unwrap();
    let exit = protocol
        .decode_exit(
            &request,
            ready_frame.response().preparation_id(),
            exit.header,
            &exit.payload,
            exit.descriptors.len(),
        )
        .unwrap();
    assert_eq!(exit.report().reason(), ExitReason::ClientClosed);
    assert!(exit.report().is_final());
    assert_eq!(child.control.read(&mut [0]).unwrap(), 0);
    child.wait();
}
