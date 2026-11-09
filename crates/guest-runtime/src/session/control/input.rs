use core::task::Poll;
use lasper_guest_protocol::ControlFrameHeader;

use super::super::error::SystemResultExt;
use super::super::ready::IO_TIMEOUT_MS;
use super::super::GuardianError;
use crate::memory::Mapping;
use crate::transport::{
    FrameReceiver, GuestControlStream, GuestReceivedFrame, IoBudget, PayloadReceiver,
    TransportError,
};

// Header and payload are separate owners so a declared length is checked by
// transport before any mapping is allocated. A complete frame owns its mapping.
pub(super) enum Input<'a> {
    Header {
        reader: FrameReceiver<'a>,
        deadline: Option<u64>,
    },
    Payload {
        reader: PayloadReceiver<'a, Mapping>,
        deadline: u64,
    },
}

impl<'a> Input<'a> {
    pub(super) fn new(connection: &'a GuestControlStream) -> Result<Self, GuardianError> {
        Ok(Self::Header {
            reader: connection.begin_receive().map_err(GuardianError::Control)?,
            deadline: None,
        })
    }

    pub(super) fn deadline_ms(&self) -> Option<u64> {
        match self {
            Self::Header { deadline, .. } => *deadline,
            Self::Payload { deadline, .. } => Some(*deadline),
        }
    }

    pub(super) fn advance(
        self,
        now: u64,
        budget: &mut IoBudget,
    ) -> Result<InputProgress<'a>, GuardianError> {
        if self.deadline_ms().is_some_and(|deadline| now >= deadline) {
            return Err(GuardianError::ControlExpired);
        }
        match self {
            Self::Header {
                mut reader,
                mut deadline,
            } => {
                let progress = reader
                    .advance_with_budget(budget)
                    .map_err(GuardianError::Control)?;
                if reader.has_started() {
                    deadline.get_or_insert(now.saturating_add(IO_TIMEOUT_MS));
                }
                match progress {
                    Poll::Pending => Ok(InputProgress::Pending(Self::Header { reader, deadline })),
                    Poll::Ready(header) => {
                        if header.descriptor_count() != 0 {
                            return Err(GuardianError::Control(
                                TransportError::UnexpectedDescriptors,
                            ));
                        }
                        let buffer = Mapping::new((header.payload_bytes() as usize).max(1))
                            .context("allocate guardian command")?;
                        let reader = reader
                            .into_payload(buffer)
                            .map_err(GuardianError::Control)?;
                        Self::Payload {
                            reader,
                            deadline: deadline.unwrap_or(now.saturating_add(IO_TIMEOUT_MS)),
                        }
                        .advance(now, budget)
                    }
                }
            }
            Self::Payload {
                mut reader,
                deadline,
            } => match reader
                .advance_with_budget(budget)
                .map_err(GuardianError::Control)?
            {
                Poll::Pending => Ok(InputProgress::Pending(Self::Payload { reader, deadline })),
                Poll::Ready(()) => Ok(InputProgress::Complete(
                    reader.finish().map_err(GuardianError::Control)?,
                )),
            },
        }
    }
}

pub(super) enum InputProgress<'a> {
    Pending(Input<'a>),
    Complete(GuestReceivedFrame<Mapping>),
}

pub(super) fn operation(
    header: ControlFrameHeader,
) -> Option<lasper_guest_protocol::GuestOperation> {
    use lasper_guest_protocol::{GuestOperation as Operation, MessageKind};
    Some(match header.kind() {
        MessageKind::PrepareRequest => Operation::Prepare,
        MessageKind::KeepPrepared => Operation::KeepPrepared,
        MessageKind::Recheck => Operation::Recheck,
        MessageKind::Start => Operation::Start,
        MessageKind::Cancel => Operation::Cancel,
        MessageKind::Close => Operation::Close,
        _ => return None,
    })
}
