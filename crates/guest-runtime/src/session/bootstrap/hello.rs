use core::task::Poll;
use lasper_guest_protocol::{
    BrokerId, ControlFrameHeader, ControlLimits, HelloRequest, MessageKind,
    HELLO_PAYLOAD_BUFFER_BYTES,
};

use crate::linux::uapi::{
    errno::EINTR,
    poll::{POLLIN, POLLOUT},
};
use crate::linux::{self, PollFd};
use crate::readiness::GuestWorkerReadiness;
use crate::session::error::SystemResultExt;
use crate::session::GuardianError;
use crate::transport::{GuestControlStream, TransportError};

use super::supervisor::Supervisor;

pub(super) struct CompletedHello {
    pub(super) request_id: u64,
    pub(super) negotiated: lasper_guest_protocol::NegotiatedProtocol,
    pub(super) completed_at_ms: u64,
}

pub(super) fn exchange(
    connection: &mut GuestControlStream,
    supervisor: &Supervisor,
    broker: BrokerId,
    deadline_ms: u64,
) -> Result<CompletedHello, GuardianError> {
    let completed = {
        let hello = HelloExchange {
            connection,
            supervisor,
            deadline_ms,
        };
        let mut bytes = [0; HELLO_PAYLOAD_BUFFER_BYTES];
        let mut reader = connection.begin_receive().map_err(GuardianError::Control)?;
        hello.drive(POLLIN, || reader.advance())?;
        let mut reader = reader
            .into_payload(&mut bytes[..])
            .map_err(GuardianError::Control)?;
        hello.drive(POLLIN, || reader.advance())?;
        let frame = reader.finish().map_err(GuardianError::Control)?;
        let request =
            HelloRequest::decode_payload(frame.payload()).map_err(GuardianError::HelloPayload)?;
        let readiness = GuestWorkerReadiness::check().map_err(GuardianError::Readiness)?;
        let offer = readiness.offer(ControlLimits::CURRENT);
        let accepted = readiness.accept_hello(
            frame.header().protocol_version(),
            request,
            broker,
            ControlLimits::CURRENT,
        );
        let request_id = frame.header().request_id();
        drop(frame);
        let accepted = match accepted {
            Ok(value) => value,
            Err(error) => {
                let crate::readiness::GuestHelloError::Negotiation(reason) = error;
                let rejected = lasper_guest_protocol::HelloRejected { offer, reason };
                let length = rejected
                    .encode_payload(&mut bytes)
                    .map_err(GuardianError::HelloPayload)?;
                let header = ControlFrameHeader::new_with_version(
                    lasper_guest_protocol::ProtocolVersion::BOOTSTRAP,
                    MessageKind::HelloRejected,
                    request_id,
                    length as u32,
                    0,
                )
                .map_err(|e| GuardianError::Control(TransportError::InvalidHeader(e)))?;
                let mut writer = connection
                    .begin_send(header, &bytes[..length], &[])
                    .map_err(GuardianError::Control)?;
                hello.drive(POLLOUT, || writer.advance())?;
                return Err(GuardianError::Hello(error));
            }
        };
        let length = accepted
            .response()
            .encode_payload(&mut bytes)
            .map_err(GuardianError::HelloPayload)?;
        let header = ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion::BOOTSTRAP,
            MessageKind::HelloResponse,
            request_id,
            length as u32,
            0,
        )
        .map_err(|error| GuardianError::Control(TransportError::InvalidHeader(error)))?;
        let mut writer = connection
            .begin_send(header, &bytes[..length], &[])
            .map_err(GuardianError::Control)?;
        hello.drive(POLLOUT, || writer.advance())?;
        CompletedHello {
            request_id,
            negotiated: accepted.negotiated(),
            completed_at_ms: hello.check()?,
        }
    };
    connection
        .complete_protocol(completed.negotiated)
        .map_err(GuardianError::Control)?;
    Ok(completed)
}

struct HelloExchange<'a> {
    connection: &'a GuestControlStream,
    supervisor: &'a Supervisor,
    deadline_ms: u64,
}

impl HelloExchange<'_> {
    fn check(&self) -> Result<u64, GuardianError> {
        self.supervisor.ensure_alive()?;
        let now = linux::monotonic_ms().context("read guardian Hello clock")?;
        if now >= self.deadline_ms {
            return Err(GuardianError::HelloExpired);
        }
        Ok(now)
    }

    fn drive<T>(
        &self,
        events: i16,
        mut advance: impl FnMut() -> Result<Poll<T>, TransportError>,
    ) -> Result<T, GuardianError> {
        loop {
            self.check()?;
            match advance().map_err(GuardianError::Control)? {
                Poll::Ready(value) => {
                    self.check()?;
                    return Ok(value);
                }
                Poll::Pending => {
                    let now = self.check()?;
                    let [broker, signals] = self.supervisor.descriptors();
                    let mut control = PollFd::new(self.connection.as_raw_fd());
                    control.events = events;
                    let mut descriptors = [control, PollFd::new(broker), PollFd::new(signals)];
                    // Every wait uses the original accept deadline, including
                    // partial requests and a response stalled on POLLOUT.
                    match linux::poll(&mut descriptors, Some(self.deadline_ms - now)) {
                        Err(EINTR) => continue,
                        result => result.context("wait for guardian Hello")?,
                    }
                }
            }
        }
    }
}
