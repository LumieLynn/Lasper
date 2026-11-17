//! Exchanges Prepare and validates Ready before identity observation begins.
//!
//! The returned wire state and conservative lease estimate seed the event loop.

use crate::session::{wire, BrokerSessionError};
use crate::{BrokerConnection, PreparedWorker};
use lasper_guest_protocol::{ControlFrameHeader, MessageKind, PrepareRequestFrame};
use std::sync::Arc;
use tokio::time::Instant;

pub(super) async fn exchange<'payload>(
    connection: &mut BrokerConnection,
    request: Arc<PrepareRequestFrame<'payload>>,
    header: ControlFrameHeader,
    payload: &[u8],
    deadline: Instant,
) -> Result<
    (
        PreparedWorker<'payload>,
        Instant,
        lasper_guest_protocol::WireSessionState,
    ),
    BrokerSessionError,
> {
    // The guest's remaining lease may age in transit. Counting from the send
    // keeps the host's estimate conservative when Ready is delayed.
    let sent_at = Instant::now();
    let mut wire_state = connection.session_state();
    wire_state
        .request(
            header.request_id(),
            lasper_guest_protocol::GuestOperation::Prepare,
        )
        .map_err(wire::sequence)?;
    wire::send(connection, header, payload, deadline).await?;
    let frame = wire::receive(connection, deadline).await?;
    if frame.header().kind() == MessageKind::Error {
        if let Err(error) = wire::rejection(
            &connection.hello().protocol(),
            header,
            &request,
            None,
            &frame,
        ) {
            if matches!(error, BrokerSessionError::Rejected(_)) {
                wire_state
                    .reply(
                        header.request_id(),
                        lasper_guest_protocol::GuestOperation::Prepare,
                        lasper_guest_protocol::WireReply::Rejected,
                    )
                    .map_err(wire::sequence)?;
            }
            return Err(error);
        }
    }
    let worker = PreparedWorker::from_frame(
        Arc::clone(connection.target()),
        connection.readiness(),
        request,
        frame,
    )?;
    let lease_deadline = sent_at
        + std::time::Duration::from_millis(u64::from(
            worker.ready().response().remaining_lease_ms(),
        ));
    wire_state
        .reply(
            header.request_id(),
            lasper_guest_protocol::GuestOperation::Prepare,
            lasper_guest_protocol::WireReply::Ready,
        )
        .map_err(wire::sequence)?;
    Ok((worker, lease_deadline, wire_state))
}
