extern crate std;

use super::super::{
    driver,
    input::{Input, InputProgress},
    prepare, test_fixture as fixture,
};
use super::*;
use crate::transport::IoBudget;
use crate::{
    linux,
    session::{worker::clock_fixture, GuardianContext, SessionPhase},
};
use lasper_guest_protocol::*;
use std::{io::Read, os::fd::AsRawFd, time::Duration};

fn expired_at_dispatch(context: GuardianContext) -> i32 {
    let run = || -> Result<(), GuardianError> {
        let mut wire = WireSessionState::after_hello(context.hello_request_id());
        let storage = prepare::receive(&context, &mut wire)?;
        let request = storage
            .request_with(context.negotiated_protocol())
            .map_err(GuardianError::PrepareStorage)?;
        let now = linux::monotonic_ms().map_err(|errno| GuardianError::System {
            operation: "read dispatch test clock",
            errno,
        })?;
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
            let _ = linux::poll(&mut [], Some(1));
        }
        let mut input = Input::new(context.connection())?;
        let frame = loop {
            match input.advance(now, &mut IoBudget::new())? {
                InputProgress::Complete(frame) => break frame,
                InputProgress::Pending(next) => input = next,
            }
            let _ = linux::poll(&mut [], Some(1));
        };
        // The driver's earlier observation was AwaitingStart. Expire the lease
        // precisely when the operation refreshes the worker and lifecycle.
        let expired = guardian
            .next_deadline_ms()
            .ok_or(GuardianError::InvalidStatus)?;
        if dispatch(
            &mut guardian,
            &request,
            preparation,
            &mut wire,
            frame,
            expired,
        )?
        .is_some()
            || guardian.has_reply()
        {
            return Err(GuardianError::InvalidStatus);
        }
        guardian.force_cleanup(CleanupReason::LeaseExpired);
        loop {
            let observation = guardian.poll(expired)?;
            if observation.cleanup_reason != Some(CleanupReason::LeaseExpired) {
                return Err(GuardianError::InvalidStatus);
            }
            if observation.phase == SessionPhase::Closed {
                break;
            }
            let _ = linux::poll(&mut [], Some(1));
        }
        driver::run(&context, &request, preparation, &mut guardian, wire)
    };
    run().map_or(71, |()| 0)
}

#[test]
fn expiry_during_dispatch_sends_exit_without_a_late_operation_reply() {
    #[derive(Clone, Copy, Debug)]
    enum RequestVariant {
        Valid,
        MalformedPayload,
        ReusedId,
    }
    for variant in [
        RequestVariant::Valid,
        RequestVariant::MalformedPayload,
        RequestVariant::ReusedId,
    ] {
        for operation in [
            GuestOperation::KeepPrepared,
            GuestOperation::Recheck,
            GuestOperation::Cancel,
            GuestOperation::Close,
            GuestOperation::Start,
        ] {
            let mut child = fixture::ForkedGuardian::spawn(expired_at_dispatch);
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
            let frame = fixture::wire::receive(child.control.as_raw_fd()).unwrap();
            let ready = protocol
                .decode_ready(
                    &request,
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            let preparation = ready.response().preparation_id();
            let (header, length) = if operation == GuestOperation::Start {
                protocol
                    .encode_start_frame(
                        &StartFrame::new(
                            11,
                            StartRequest::new(&request, preparation, None, None).unwrap(),
                        ),
                        &mut bytes,
                    )
                    .unwrap()
            } else {
                let kind = match operation {
                    GuestOperation::KeepPrepared => PreparationControlKind::KeepPrepared,
                    GuestOperation::Recheck => PreparationControlKind::Recheck,
                    GuestOperation::Cancel => PreparationControlKind::Cancel,
                    GuestOperation::Close => PreparationControlKind::Close,
                    _ => unreachable!(),
                };
                protocol
                    .encode_control_request(
                        &PreparationControlFrame::new(
                            11,
                            kind,
                            PreparationControl::new(request.broker_id(), preparation),
                        ),
                        &mut bytes,
                    )
                    .unwrap()
            };
            let (header, payload) = match variant {
                RequestVariant::Valid => (header, &bytes[..length]),
                RequestVariant::MalformedPayload => (
                    ControlFrameHeader::new_with_version(
                        header.protocol_version(),
                        header.kind(),
                        11,
                        1,
                        0,
                    )
                    .unwrap(),
                    b"{".as_slice(),
                ),
                RequestVariant::ReusedId => (
                    ControlFrameHeader::new_with_version(
                        header.protocol_version(),
                        header.kind(),
                        request.request_id(),
                        header.payload_bytes(),
                        0,
                    )
                    .unwrap(),
                    &bytes[..length],
                ),
            };
            fixture::wire::send(child.control.as_raw_fd(), header, payload, &[]).unwrap();
            let frame = fixture::wire::receive(child.control.as_raw_fd()).unwrap();
            let exit = protocol
                .decode_exit(
                    &request,
                    preparation,
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            assert_eq!(
                exit.report().reason(),
                ExitReason::LeaseExpired,
                "{operation:?}, {variant:?}"
            );
            assert!(exit.report().is_final());
            assert_eq!(exit.report().execution(), ExecutionStatus::NotStarted);
            assert_eq!(child.control.read(&mut [0]).unwrap(), 0);
            child.wait();
        }
    }
}
