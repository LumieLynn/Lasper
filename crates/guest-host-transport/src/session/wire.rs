use super::BrokerSessionError;
use crate::{BrokerConnection, ReceivedControlFrame};
use lasper_guest_protocol::{
    ControlFrameHeader, ErrorFrame, MessageKind, PayloadCodecError, PreparationId,
    PrepareRequestFrame,
};
use std::fmt;
use tokio::time::{timeout_at, Instant};

pub(super) fn encoding(error: impl fmt::Debug) -> BrokerSessionError {
    BrokerSessionError::InvalidRequest(format!("{error:?}"))
}

pub(super) fn encode(
    maximum: usize,
    mut encode: impl FnMut(&mut [u8]) -> Result<usize, PayloadCodecError>,
) -> Result<Box<[u8]>, BrokerSessionError> {
    let mut capacity = 4096.min(maximum);
    loop {
        let mut payload = vec![0; capacity];
        match encode(&mut payload) {
            Ok(length) => {
                payload.truncate(length);
                // Prepare retains this allocation for the session. Keep spare
                // encoding capacity out of that long-lived request.
                return Ok(payload.into_boxed_slice());
            }
            Err(PayloadCodecError::OutputTooSmall) if capacity < maximum => {
                capacity = (capacity * 2).min(maximum);
            }
            Err(error) => return Err(encoding(error)),
        }
    }
}

pub(super) async fn send(
    connection: &mut BrokerConnection,
    header: ControlFrameHeader,
    payload: &[u8],
    deadline: Instant,
) -> Result<(), BrokerSessionError> {
    if Instant::now() >= deadline {
        return Err(BrokerSessionError::Deadline);
    }
    connection.recheck()?;
    timeout_at(
        deadline,
        connection.control().send_frame(header, payload, &[]),
    )
    .await
    .map_err(|_| BrokerSessionError::Deadline)??;
    Ok(())
}

pub(super) async fn receive(
    connection: &mut BrokerConnection,
    deadline: Instant,
) -> Result<ReceivedControlFrame, BrokerSessionError> {
    if Instant::now() >= deadline {
        return Err(BrokerSessionError::Deadline);
    }
    Ok(timeout_at(deadline, connection.control().receive_frame())
        .await
        .map_err(|_| BrokerSessionError::Deadline)??)
}

pub(super) fn rejection(
    request_header: ControlFrameHeader,
    request: &PrepareRequestFrame<'_>,
    preparation: Option<PreparationId>,
    frame: &ReceivedControlFrame,
) -> Result<(), BrokerSessionError> {
    if frame.header().kind() == MessageKind::Error {
        return Err(BrokerSessionError::Rejected(ErrorFrame::decode_for(
            request_header,
            request,
            preparation,
            frame.header(),
            frame.payload(),
            frame.descriptors().len(),
        )?));
    }
    Ok(())
}
