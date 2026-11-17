//! Connection codecs apply the selected profile, effective limits and feature
//! gates. They validate message contents; WireSessionState validates ordering.

use super::dispatch::validate_timeout;
use super::fields::FieldPresence;
use crate::profile::{Direction, NegotiatedProtocol};
use crate::{
    ControlFrameHeader, ControlReplyFrame, ErrorFrame, ExitFrame, MessageKind, PayloadCodecError,
    PreparationControlFrame, PreparationId, PrepareFrameError, PrepareReadyFrame, PrepareRequest,
    PrepareRequestFrame, SessionFrameError, SessionPayloadError, StartAcceptedFrame, StartFrame,
    StartRequest,
};

macro_rules! negotiated_session_codec {
    ($encode:ident, $decode:ident, $frame:ty, $direction:ident, ($($arg:ident: $ty:ty),*)) => {
        impl NegotiatedProtocol {
            pub fn $encode(&self, frame: &$frame, payload: &mut [u8])
                -> Result<(ControlFrameHeader, usize), SessionFrameError>
            {
                let encoded = self.profile().$encode(frame, payload)?;
                self.validate_session_header(encoded.0, Direction::$direction)?;
                self.validate_fields(encoded.0.kind(), Direction::$direction, frame)
                    .map_err(SessionFrameError::Payload)?;
                Ok(encoded)
            }

            pub fn $decode(&self, $($arg: $ty,)* header: ControlFrameHeader,
                payload: &[u8], descriptors: usize) -> Result<$frame, SessionFrameError>
            {
                self.validate_session_header(header, Direction::$direction)?;
                let frame = self.profile().$decode($($arg,)* header, payload, descriptors)?;
                self.validate_fields(header.kind(), Direction::$direction, &frame)
                    .map_err(SessionFrameError::Payload)?;
                Ok(frame)
            }
        }
    };
}

negotiated_session_codec!(encode_control_request, decode_control_request,
    PreparationControlFrame, HostToGuest,
    (request_id: u64, request: &PrepareRequestFrame<'_>, preparation: PreparationId));
negotiated_session_codec!(encode_control_reply, decode_control_reply,
    ControlReplyFrame, GuestToHost, (command: &PreparationControlFrame));
negotiated_session_codec!(encode_start_accepted, decode_start_accepted,
    StartAcceptedFrame, GuestToHost,
    (request_id: u64, request: &PrepareRequestFrame<'_>, preparation: PreparationId));
negotiated_session_codec!(encode_exit, decode_exit, ExitFrame, GuestToHost,
    (request: &PrepareRequestFrame<'_>, preparation: PreparationId));
negotiated_session_codec!(encode_error, decode_error, ErrorFrame, GuestToHost,
    (request_header: ControlFrameHeader, request: &PrepareRequestFrame<'_>, preparation: Option<PreparationId>));

impl NegotiatedProtocol {
    fn validate_payload_size(&self, length: usize) -> Result<(), PayloadCodecError> {
        let maximum =
            self.limits()
                .get()
                .max_control_frame_bytes()
                .min(self.profile().capacities().max_control_frame_bytes) as usize;
        if length > maximum.saturating_sub(crate::CONTROL_FRAME_HEADER_BYTES) {
            return Err(PayloadCodecError::PayloadTooLarge);
        }
        Ok(())
    }

    /// Encodes only the JSON payload. Its size must leave room for the control
    /// header within this connection's frame budget.
    pub fn encode_prepare(
        &self,
        request: &PrepareRequest<'_>,
        output: &mut [u8],
    ) -> Result<usize, PayloadCodecError> {
        self.validate_fields(MessageKind::PrepareRequest, Direction::HostToGuest, request)
            .map_err(|_| PayloadCodecError::InvalidRequest)?;
        let length = self.profile().encode_prepare(request, output)?;
        self.validate_payload_size(length)?;
        Ok(length)
    }

    /// Decoded strings borrow the payload, so scratch can be reused immediately.
    /// This payload-only decoder does not check a header or received FD count.
    pub fn decode_prepare<'a>(
        &self,
        payload: &'a [u8],
        scratch: &mut [u8],
    ) -> Result<PrepareRequest<'a>, PayloadCodecError> {
        self.validate_payload_size(payload.len())?;
        let request = self.profile().decode_prepare(payload, scratch)?;
        self.validate_fields(
            MessageKind::PrepareRequest,
            Direction::HostToGuest,
            &request,
        )
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
        Ok(request)
    }

    pub fn encode_start_payload(
        &self,
        start: &StartRequest,
        output: &mut [u8],
    ) -> Result<usize, PayloadCodecError> {
        self.validate_start(start)
            .map_err(|_| PayloadCodecError::InvalidRequest)?;
        let length = self.profile().encode_start_payload(start, output)?;
        self.validate_payload_size(length)?;
        Ok(length)
    }

    pub fn encode_prepare_frame(
        &self,
        frame: &PrepareRequestFrame<'_>,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        self.validate_prepare_fields(MessageKind::PrepareRequest, Direction::HostToGuest, frame)?;
        let length = self.profile().encode_prepare_frame(frame, output)?;
        let header = ControlFrameHeader::decode(&output[..crate::CONTROL_FRAME_HEADER_BYTES])
            .map_err(PrepareFrameError::Header)?;
        self.validate_prepare_header(header, Direction::HostToGuest)?;
        Ok(length)
    }

    fn validate_prepare_header(
        &self,
        header: ControlFrameHeader,
        direction: Direction,
    ) -> Result<(), PrepareFrameError> {
        self.profile().validate_prepare_header(header)?;
        self.limits()
            .get()
            .validate_header(header)
            .map_err(PrepareFrameError::Limit)?;
        let required = self
            .profile()
            .schema(header.kind(), direction)
            .ok_or(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            ))?
            .required_features;
        if !self.features().contains(required) {
            return Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            ));
        }
        Ok(())
    }

    pub fn encode_ready(
        &self,
        frame: PrepareReadyFrame,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        self.validate_prepare_fields(MessageKind::PrepareReady, Direction::GuestToHost, &frame)?;
        let length = self.profile().encode_ready(frame, output)?;
        let header = ControlFrameHeader::decode(&output[..crate::CONTROL_FRAME_HEADER_BYTES])
            .map_err(PrepareFrameError::Header)?;
        self.validate_prepare_header(header, Direction::GuestToHost)?;
        Ok(length)
    }

    pub fn decode_ready(
        &self,
        request: &PrepareRequestFrame<'_>,
        header: ControlFrameHeader,
        payload: &[u8],
        descriptors: usize,
    ) -> Result<PrepareReadyFrame, PrepareFrameError> {
        self.validate_prepare_header(header, Direction::GuestToHost)?;
        let frame = self
            .profile()
            .decode_ready(request, header, payload, descriptors)?;
        self.validate_prepare_fields(MessageKind::PrepareReady, Direction::GuestToHost, &frame)?;
        Ok(frame)
    }

    /// Decodes into caller-owned metadata to avoid a large stack temporary.
    /// Strings borrow the payload; scratch is reusable after this call. Any
    /// failure invalidates the frame previously held in storage.
    pub fn decode_prepare_into<'a, 's>(
        &self,
        storage: &'s mut crate::PrepareDecodeStorage<'a>,
        header: ControlFrameHeader,
        payload: &'a [u8],
        descriptors: usize,
        broker: crate::BrokerId,
        scratch: &mut [u8],
    ) -> Result<&'s PrepareRequestFrame<'a>, PrepareFrameError> {
        storage.invalidate();
        self.validate_prepare_header(header, Direction::HostToGuest)?;
        let frame = self.profile().decode_prepare_into(
            storage,
            header,
            payload,
            descriptors,
            broker,
            scratch,
        )?;
        if let Err(error) =
            self.validate_prepare_fields(MessageKind::PrepareRequest, Direction::HostToGuest, frame)
        {
            storage.invalidate();
            return Err(error);
        }
        Ok(storage.request().expect("validated Prepare frame"))
    }

    pub fn decode_prepare_frame<'a>(
        &self,
        header: ControlFrameHeader,
        payload: &'a [u8],
        descriptors: usize,
        broker: crate::BrokerId,
        scratch: &mut [u8],
    ) -> Result<PrepareRequestFrame<'a>, PrepareFrameError> {
        let mut storage = crate::PrepareDecodeStorage::new();
        self.decode_prepare_into(&mut storage, header, payload, descriptors, broker, scratch)?;
        Ok(storage.into_frame())
    }

    fn validate_session_header(
        &self,
        header: ControlFrameHeader,
        direction: Direction,
    ) -> Result<(), SessionFrameError> {
        self.profile().validate_session_header(header)?;
        self.limits()
            .get()
            .validate_header(header)
            .map_err(SessionFrameError::Limit)?;
        let schema = self.profile().schema(header.kind(), direction).ok_or(
            SessionFrameError::UnexpectedMessageKind {
                actual: header.kind(),
            },
        )?;
        if !self.features().contains(schema.required_features) {
            return Err(SessionFrameError::Payload(
                SessionPayloadError::FeatureNotNegotiated,
            ));
        }
        Ok(())
    }

    fn validate_fields(
        &self,
        kind: MessageKind,
        direction: Direction,
        frame: &impl FieldPresence,
    ) -> Result<(), SessionPayloadError> {
        let required = self
            .profile()
            .required_features_for_fields(kind, direction, |path| {
                #[cfg(test)]
                if path == "test_marker"
                    && crate::profile::schema_fixture::has_marker(self.profile(), kind, direction)
                {
                    return Some(true);
                }
                frame.field_present(path)
            })?;
        if !self.features().contains(required) {
            return Err(SessionPayloadError::FeatureNotNegotiated);
        }
        Ok(())
    }

    fn validate_prepare_fields(
        &self,
        kind: MessageKind,
        direction: Direction,
        frame: &impl FieldPresence,
    ) -> Result<(), PrepareFrameError> {
        self.validate_fields(kind, direction, frame)
            .map_err(|_| PrepareFrameError::Payload(PayloadCodecError::InvalidRequest))
    }

    pub fn validate_start(&self, start: &StartRequest) -> Result<(), SessionPayloadError> {
        self.validate_fields(MessageKind::Start, Direction::HostToGuest, start)?;
        validate_timeout(start, self.profile().capacities())
    }

    pub fn encode_start_frame(
        &self,
        frame: &StartFrame,
        output: &mut [u8],
    ) -> Result<(ControlFrameHeader, usize), SessionFrameError> {
        self.validate_start(frame.request())
            .map_err(SessionFrameError::Payload)?;
        let encoded = self.profile().encode_start(frame, output)?;
        self.validate_session_header(encoded.0, Direction::HostToGuest)?;
        Ok(encoded)
    }

    pub fn decode_start_frame(
        &self,
        request_id: u64,
        request: &PrepareRequestFrame<'_>,
        preparation: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        descriptors: usize,
    ) -> Result<StartFrame, SessionFrameError> {
        self.validate_session_header(header, Direction::HostToGuest)?;
        let frame = self.profile().decode_start_frame(
            request_id,
            request,
            preparation,
            header,
            payload,
            descriptors,
        )?;
        self.validate_start(frame.request())
            .map_err(SessionFrameError::Payload)?;
        Ok(frame)
    }

    pub fn decode_start(
        &self,
        request: &PrepareRequestFrame<'_>,
        preparation: PreparationId,
        payload: &[u8],
    ) -> Result<StartRequest, SessionPayloadError> {
        self.validate_payload_size(payload.len())
            .map_err(SessionPayloadError::Payload)?;
        let start = self
            .profile()
            .decode_start_payload(request, preparation, payload)?;
        self.validate_start(&start)?;
        Ok(start)
    }
}
