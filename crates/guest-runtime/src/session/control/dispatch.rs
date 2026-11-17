use super::super::reply::{Payload, ReplyKind};
use super::super::{CleanupReason, GuardianError, SessionAction, SessionGuardian};
use crate::memory::Mapping;
use crate::transport::GuestReceivedFrame;
use lasper_guest_protocol::{
    ControlReplyFrame, ControlReplyStatus, ErrorFrame, GuestErrorCode, GuestOperation,
    PreparationControlFrame, PreparationId, PrepareRequestFrame, StartFrame,
};

enum Command {
    Prepare,
    Start(StartFrame),
    Control(PreparationControlFrame),
}

pub(super) fn dispatch(
    guardian: &mut SessionGuardian<'_, '_>,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    wire: &mut lasper_guest_protocol::WireSessionState,
    frame: GuestReceivedFrame<Mapping>,
    now: u64,
) -> Result<Option<(u64, GuestOperation, lasper_guest_protocol::WireReply)>, GuardianError> {
    // Cleanup takes precedence even when the pending request has an invalid
    // payload or ID and would never reach start() or control().
    guardian.poll(now)?;
    if !guardian.accepts_commands() {
        return Ok(None);
    }
    let header = frame.header();
    let protocol = guardian.negotiated_protocol();
    let operation = super::input::operation(header).ok_or(GuardianError::InvalidStatus)?;
    // Invalid fields or binding are request errors even when the operation is
    // also out of sequence. Register its ID separately so rejection cannot
    // restore Start eligibility or allow an ID to be reused.
    let command = decode_command(protocol, request, preparation, &frame, operation);
    let rejection = match wire.request(header.request_id(), operation) {
        Ok(()) => None,
        Err(lasper_guest_protocol::WireSessionError::InvalidRequestId) => {
            Some(GuestErrorCode::InvalidRequest)
        }
        Err(lasper_guest_protocol::WireSessionError::InvalidState { .. }) => {
            Some(GuestErrorCode::InvalidState)
        }
        Err(_) => return Err(GuardianError::InvalidStatus),
    };
    let response = if let Err(error) = command {
        Err(error)
    } else if let Some(code) = rejection {
        Err((code, None))
    } else {
        match command {
            Ok(Command::Start(start)) => match guardian.start(now, start) {
                Ok(accepted) => Ok(Payload::encode(4096, |buffer| {
                    protocol
                        .encode_start_accepted(&accepted, buffer)
                        .map_err(GuardianError::SessionFrame)
                })?),
                Err(_) if !guardian.accepts_commands() => return Ok(None),
                Err(error) => Err(classify(error)),
            },
            Ok(Command::Control(command)) => match guardian.control(now, command) {
                Ok(SessionAction::Recheck | SessionAction::LeaseRenewed { .. }) => {
                    let status = ControlReplyStatus::Prepared {
                        remaining_lease_ms: guardian.remaining_lease_ms(now),
                    };
                    encode_control(protocol, command, status)
                }
                Ok(SessionAction::BeginCleanup(
                    CleanupReason::Cancelled | CleanupReason::ClientClosed,
                )) => encode_control(protocol, command, ControlReplyStatus::CleanupRequested),
                // Polling inside the operation can discover expiry or worker
                // exit after the driver's earlier observation. Only the Exit
                // report remains due once that cleanup has already won.
                Ok(SessionAction::Noop | SessionAction::BeginCleanup(_)) => return Ok(None),
                Ok(_) => return Err(GuardianError::InvalidStatus),
                Err(_) if !guardian.accepts_commands() => return Ok(None),
                Err(error) => Err(classify(error)),
            },
            Ok(Command::Prepare) | Err(_) => return Err(GuardianError::InvalidStatus),
        }
    };
    let (response, result) = match response {
        Ok(response) => {
            let reply = match operation {
                GuestOperation::Start => lasper_guest_protocol::WireReply::StartAccepted,
                GuestOperation::KeepPrepared | GuestOperation::Recheck => {
                    lasper_guest_protocol::WireReply::Prepared
                }
                GuestOperation::Cancel | GuestOperation::Close => {
                    lasper_guest_protocol::WireReply::CleanupRequested
                }
                GuestOperation::Prepare => return Err(GuardianError::InvalidStatus),
            };
            (response, reply)
        }
        Err((code, errno)) => {
            let error = ErrorFrame::new(
                header.request_id(),
                request.broker_id(),
                Some(preparation),
                operation,
                code,
                errno,
            )
            .map_err(|_| GuardianError::ReplyEncoding)?;
            let payload = Payload::encode(4096, |buffer| {
                protocol
                    .encode_error(&error, buffer)
                    .map_err(GuardianError::SessionFrame)
            })?;
            (payload, lasper_guest_protocol::WireReply::Rejected)
        }
    };
    guardian.queue_reply(ReplyKind::Command, now, response)?;
    Ok(rejection
        .is_none()
        .then_some((header.request_id(), operation, result)))
}

fn decode_command(
    protocol: lasper_guest_protocol::NegotiatedProtocol,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    frame: &GuestReceivedFrame<Mapping>,
    operation: GuestOperation,
) -> Result<Command, (GuestErrorCode, Option<i32>)> {
    let header = frame.header();
    match operation {
        GuestOperation::Prepare => super::super::prepare::decode_request(
            protocol,
            header,
            frame.payload(),
            request.broker_id(),
        )
        .map(|_| Command::Prepare)
        .map_err(|error| match error {
            super::super::PrepareStorageError::System(errno) => {
                (GuestErrorCode::System, Some(errno))
            }
            _ => (GuestErrorCode::InvalidRequest, None),
        }),
        GuestOperation::Start => protocol
            .decode_start_frame(
                header.request_id(),
                request,
                preparation,
                header,
                frame.payload(),
                frame.descriptors().len(),
            )
            .map(Command::Start)
            .map_err(|_| (GuestErrorCode::InvalidRequest, None)),
        _ => protocol
            .decode_control_request(
                header.request_id(),
                request,
                preparation,
                header,
                frame.payload(),
                frame.descriptors().len(),
            )
            .map(Command::Control)
            .map_err(|_| (GuestErrorCode::InvalidRequest, None)),
    }
}

fn encode_control(
    protocol: lasper_guest_protocol::NegotiatedProtocol,
    command: PreparationControlFrame,
    status: ControlReplyStatus,
) -> Result<(lasper_guest_protocol::ControlFrameHeader, Payload), (GuestErrorCode, Option<i32>)> {
    let reply =
        ControlReplyFrame::new(command, status).map_err(|_| (GuestErrorCode::Expired, None))?;
    Payload::encode(4096, |buffer| {
        protocol
            .encode_control_reply(&reply, buffer)
            .map_err(GuardianError::SessionFrame)
    })
    .map_err(classify)
}

fn classify(error: GuardianError) -> (GuestErrorCode, Option<i32>) {
    match error {
        GuardianError::System { errno, .. } => (GuestErrorCode::System, Some(errno)),
        GuardianError::NotPrepared
        | GuardianError::Lifecycle(_)
        | GuardianError::AlreadyStarted => (GuestErrorCode::InvalidState, None),
        GuardianError::InvalidStart(_) | GuardianError::SessionFrame(_) => {
            (GuestErrorCode::InvalidRequest, None)
        }
        _ => (GuestErrorCode::Unavailable, None),
    }
}

#[cfg(test)]
#[path = "../../../tests/control/dispatch.rs"]
mod tests;
