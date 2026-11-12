use crate::session::{wire, BrokerSessionError, Reply};
use crate::ReceivedControlFrame;
use lasper_guest_protocol::{
    ControlFrameHeader, ControlReplyFrame, ControlReplyStatus, PreparationControlFrame,
    PreparationId, PrepareRequestFrame, StartAcceptedFrame,
};
use std::io;
use tokio::time::Instant;

pub(super) struct Outbound {
    pub header: ControlFrameHeader,
    pub payload: Box<[u8]>,
}

#[derive(Clone, Copy)]
pub(super) enum ExpectedReply {
    Control {
        header: ControlFrameHeader,
        command: PreparationControlFrame,
    },
    Start(ControlFrameHeader),
}

pub(super) enum ReplyValue {
    Control(ControlReplyStatus),
    StartAccepted,
}

impl ExpectedReply {
    pub fn header(self) -> ControlFrameHeader {
        match self {
            Self::Control { header, .. } | Self::Start(header) => header,
        }
    }

    pub fn decode(
        self,
        request: &PrepareRequestFrame<'_>,
        preparation: PreparationId,
        frame: &ReceivedControlFrame,
    ) -> Result<ReplyValue, BrokerSessionError> {
        wire::rejection(self.header(), request, Some(preparation), frame)?;
        Ok(match self {
            Self::Control { command, .. } => ReplyValue::Control(
                ControlReplyFrame::decode_for(
                    &command,
                    frame.header(),
                    frame.payload(),
                    frame.descriptors().len(),
                )?
                .status(),
            ),
            Self::Start(header) => {
                StartAcceptedFrame::decode_for(
                    header.request_id(),
                    request,
                    preparation,
                    frame.header(),
                    frame.payload(),
                    frame.descriptors().len(),
                )?;
                ReplyValue::StartAccepted
            }
        })
    }
}

pub(super) struct PendingRequest {
    pub expected: ExpectedReply,
    pub sent_at: Instant,
    pub deadline: Instant,
    pub purpose: RequestPurpose,
}

pub(super) enum RequestPurpose {
    Keep,
    Recheck,
    Start,
    Cleanup(Option<Reply<()>>),
}

impl PendingRequest {
    pub fn reject(self, error: &BrokerSessionError) {
        if let RequestPurpose::Cleanup(Some(reply)) = self.purpose {
            let _ = reply.send(Err(error.clone()));
        }
    }
}

pub(super) struct CleanupRequest {
    pub kind: lasper_guest_protocol::PreparationControlKind,
    pub deadline: Instant,
    pub reply: Option<Reply<()>>,
}

pub(super) fn unexpected_reply() -> BrokerSessionError {
    io::Error::new(io::ErrorKind::InvalidData, "unexpected guest control reply").into()
}
