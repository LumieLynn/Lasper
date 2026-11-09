use core::task::Poll;
use lasper_guest_protocol::{
    ControlFrameHeader, EnforcedControlLimits, CONTROL_FRAME_HEADER_BYTES,
};

use crate::memory::Mapping;
use crate::transport::{FrameSender, GuestControlStream, IoBudget};

use super::error::SystemResultExt;
use super::GuardianError;

pub(super) const IO_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReplyKind {
    Ready,
    Command,
    Exit { final_report: bool },
}

pub(super) struct Payload {
    bytes: Mapping,
    offset: usize,
    length: usize,
}

impl Payload {
    pub(super) fn encode(
        capacity: usize,
        encode: impl FnOnce(&mut [u8]) -> Result<(ControlFrameHeader, usize), GuardianError>,
    ) -> Result<(ControlFrameHeader, Self), GuardianError> {
        let mut bytes = Mapping::new(capacity).context("allocate guardian reply")?;
        let (header, length) = encode(bytes.as_mut())?;
        if length > capacity {
            return Err(GuardianError::ReplyEncoding);
        }
        Ok((
            header,
            Self {
                bytes,
                offset: 0,
                length,
            },
        ))
    }

    pub(super) fn ready(
        frame: lasper_guest_protocol::PrepareReadyFrame,
    ) -> Result<(ControlFrameHeader, Self), GuardianError> {
        let mut bytes = Mapping::new(512).context("allocate Ready reply")?;
        let length = frame
            .encode_into(bytes.as_mut())
            .map_err(GuardianError::PrepareFrame)?;
        let header = ControlFrameHeader::decode(&bytes.as_ref()[..CONTROL_FRAME_HEADER_BYTES])
            .map_err(|_| GuardianError::ReplyEncoding)?;
        Ok((
            header,
            Self {
                bytes,
                offset: CONTROL_FRAME_HEADER_BYTES,
                length: length - CONTROL_FRAME_HEADER_BYTES,
            },
        ))
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.bytes.as_ref()[self.offset..self.offset + self.length]
    }
}

pub(super) struct PendingReply<'a> {
    sender: FrameSender<'a, Payload>,
    kind: ReplyKind,
    deadline_ms: u64,
}

impl<'a> PendingReply<'a> {
    pub(super) fn new(
        connection: &'a GuestControlStream,
        limits: EnforcedControlLimits,
        kind: ReplyKind,
        now_ms: u64,
        frame: (ControlFrameHeader, Payload),
        descriptors: &[i32],
    ) -> Result<Self, GuardianError> {
        limits.get().validate_header(frame.0).map_err(|error| {
            GuardianError::Control(crate::transport::TransportError::Session(
                lasper_guest_protocol::ControlSessionError::Limit(error),
            ))
        })?;
        let sender = connection
            .begin_send(frame.0, frame.1, descriptors)
            .map_err(GuardianError::Control)?;
        Ok(Self {
            sender,
            kind,
            deadline_ms: now_ms.saturating_add(IO_TIMEOUT_MS),
        })
    }

    pub(super) const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
    pub(super) const fn kind(&self) -> ReplyKind {
        self.kind
    }

    pub(super) fn advance(
        &mut self,
        now_ms: u64,
        budget: &mut IoBudget,
    ) -> Result<Poll<ReplyKind>, GuardianError> {
        if now_ms >= self.deadline_ms {
            return Err(GuardianError::ControlExpired);
        }
        self.sender
            .advance_with_budget(budget)
            .map(|progress| progress.map(|()| self.kind))
            .map_err(GuardianError::Control)
    }
}
