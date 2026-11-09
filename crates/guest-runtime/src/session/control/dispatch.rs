use super::super::ready::{Payload, ReplyKind};
use super::super::{GuardianError, SessionAction, SessionGuardian};
use crate::memory::Mapping;
use crate::transport::GuestReceivedFrame;
use lasper_guest_protocol::{
    ControlReplyFrame, ControlReplyStatus, ErrorFrame, GuestErrorCode, GuestOperation, MessageKind,
    PreparationControlFrame, PreparationControlKind, PreparationId, PrepareRequestFrame,
    StartFrame,
};

pub(super) fn dispatch(
    guardian: &mut SessionGuardian<'_, '_>,
    request: &PrepareRequestFrame<'_>,
    preparation: PreparationId,
    last_id: &mut u64,
    frame: GuestReceivedFrame<Mapping>,
    now: u64,
) -> Result<(), GuardianError> {
    let header = frame.header();
    let operation = super::input::operation(header).ok_or(GuardianError::InvalidStatus)?;
    let rejection = if header.request_id() <= *last_id {
        Some(GuestErrorCode::InvalidRequest)
    } else {
        *last_id = header.request_id();
        if header.kind() == MessageKind::PrepareRequest {
            Some(GuestErrorCode::InvalidState)
        } else {
            None
        }
    };
    let response = if let Some(code) = rejection {
        Err((code, None))
    } else if operation == GuestOperation::Start {
        match StartFrame::decode_for(
            header.request_id(),
            request,
            preparation,
            header,
            frame.payload(),
            frame.descriptors().len(),
        ) {
            Ok(start) => match guardian.start(now, start) {
                Ok(accepted) => Ok(Payload::encode(4096, |buffer| {
                    accepted
                        .encode_parts(buffer)
                        .map_err(GuardianError::SessionFrame)
                })?),
                Err(error) => Err(classify(error)),
            },
            Err(_) => Err((GuestErrorCode::InvalidRequest, None)),
        }
    } else {
        match PreparationControlFrame::decode_for(
            header.request_id(),
            request,
            preparation,
            header,
            frame.payload(),
            frame.descriptors().len(),
        ) {
            Ok(command) => match guardian.control(now, command) {
                Ok(SessionAction::Recheck | SessionAction::LeaseRenewed { .. }) => {
                    let status = ControlReplyStatus::Prepared {
                        remaining_lease_ms: guardian.remaining_lease_ms(now),
                    };
                    encode_control(command, status)
                }
                Ok(_)
                    if matches!(
                        command.kind(),
                        PreparationControlKind::Cancel | PreparationControlKind::Close
                    ) =>
                {
                    encode_control(command, ControlReplyStatus::CleanupRequested)
                }
                Ok(_) => Err((GuestErrorCode::Expired, None)),
                Err(error) => Err(classify(error)),
            },
            Err(_) => Err((GuestErrorCode::InvalidRequest, None)),
        }
    };
    let response = match response {
        Ok(response) => response,
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
            Payload::encode(4096, |buffer| {
                error
                    .encode_parts(buffer)
                    .map_err(GuardianError::SessionFrame)
            })?
        }
    };
    guardian.queue_reply(ReplyKind::Command, now, response)
}

fn encode_control(
    command: PreparationControlFrame,
    status: ControlReplyStatus,
) -> Result<(lasper_guest_protocol::ControlFrameHeader, Payload), (GuestErrorCode, Option<i32>)> {
    let reply =
        ControlReplyFrame::new(command, status).map_err(|_| (GuestErrorCode::Expired, None))?;
    Payload::encode(4096, |buffer| {
        reply
            .encode_parts(buffer)
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
