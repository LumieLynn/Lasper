use super::BrokerSessionError;
use crate::{BrokerConnection, ReceivedControlFrame};
use lasper_guest_protocol::{
    ControlFrameHeader, ErrorFrame, MessageKind, PayloadCodecError, PreparationId,
    PrepareRequestFrame, MAX_EXECUTION_TIMEOUT_MS,
};
use std::fmt;
use tokio::time::{timeout_at, Duration, Instant};

pub(super) fn execution_timeout_ms(duration: Duration) -> Result<u64, BrokerSessionError> {
    if duration.is_zero() {
        return Err(BrokerSessionError::InvalidRequest(
            "execution timeout must be positive".into(),
        ));
    }
    let milliseconds = duration.as_nanos().div_ceil(1_000_000);
    if milliseconds > u128::from(MAX_EXECUTION_TIMEOUT_MS) {
        return Err(BrokerSessionError::InvalidRequest(format!(
            "execution timeout exceeds the protocol limit of {MAX_EXECUTION_TIMEOUT_MS} milliseconds"
        )));
    }
    Ok(milliseconds as u64)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_budgets_round_up_without_wrapping() {
        for (duration, expected) in [
            (Duration::from_nanos(1), 1),
            (Duration::from_millis(1), 1),
            (Duration::from_nanos(1_000_001), 2),
            (Duration::from_secs(120), 120_000),
            (
                Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS) - Duration::from_nanos(1),
                MAX_EXECUTION_TIMEOUT_MS,
            ),
            (
                Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS),
                MAX_EXECUTION_TIMEOUT_MS,
            ),
        ] {
            assert_eq!(execution_timeout_ms(duration).unwrap(), expected);
        }
        for duration in [
            Duration::ZERO,
            Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS) + Duration::from_nanos(1),
            Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS + 1),
            Duration::from_millis(u64::MAX),
            Duration::from_millis(u64::MAX) + Duration::from_nanos(1),
            Duration::MAX,
        ] {
            assert!(matches!(
                execution_timeout_ms(duration),
                Err(BrokerSessionError::InvalidRequest(_))
            ));
        }
    }
}
