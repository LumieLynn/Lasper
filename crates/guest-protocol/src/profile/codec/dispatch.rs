//! Schema dispatch and per-profile capacity checks. These internal codecs do
//! not establish connection safety; public session codecs live in `negotiated`.

use crate::profile::{Direction, ProfileCapacities, ProtocolProfile, SchemaId};
use crate::{
    ControlFrameHeader, ControlReplyFrame, ErrorFrame, ExitFrame, MessageKind, PayloadCodecError,
    PreparationControlFrame, PreparationId, PrepareFrameError, PrepareReadyFrame, PrepareRequest,
    PrepareRequestFrame, SessionFrameError, SessionPayloadError, StartAcceptedFrame, StartFrame,
    StartRequest,
};

macro_rules! session_codec {
    ($encode:ident, $decode:ident, $codec:ident, $frame:ty, $direction:ident,
     $kind:expr, ($($arg:ident: $ty:ty),*), $decode_frame:expr) => {
        session_codec!($encode, $decode, $codec, $frame, $direction, $kind,
            ($($arg: $ty),*), $decode_frame,
            |_: &$frame, _: &ProfileCapacities| Ok(()));
    };
    ($encode:ident, $decode:ident, $codec:ident, $frame:ty, $direction:ident,
     $kind:expr, ($($arg:ident: $ty:ty),*), $decode_frame:expr, $validate:expr) => {
        impl ProtocolProfile {
            pub(crate) fn $encode(&self, frame: &$frame, payload: &mut [u8])
                -> Result<(ControlFrameHeader, usize), SessionFrameError>
            {
                let kind = ($kind)(frame);
                #[cfg(test)]
                if let Some(encoded) = fixture_encode!($codec, self, frame, payload) {
                    return encoded;
                }
                match self.schema(kind, Direction::$direction).map(|schema| schema.codec) {
                    Some(SchemaId::$codec) => {
                        ($validate)(frame, self.capacities()).map_err(SessionFrameError::Payload)?;
                        let encoded = frame.encode_parts_with_version(self.version(), payload)?;
                        self.validate_session_header(encoded.0)?;
                        Ok(encoded)
                    }
                    _ => Err(unsupported_schema()),
                }
            }

            pub(crate) fn $decode(&self, $($arg: $ty,)* header: ControlFrameHeader,
                payload: &[u8], descriptors: usize) -> Result<$frame, SessionFrameError>
            {
                self.validate_session_header(header)?;
                #[cfg(test)]
                if let Some(decoded) = fixture_decode!($codec, self, ($($arg),*), header, payload, descriptors) {
                    return decoded;
                }
                match self.schema(header.kind(), Direction::$direction).map(|schema| schema.codec) {
                    Some(SchemaId::$codec) => {
                        let frame = ($decode_frame)($($arg,)* header, payload, descriptors)?;
                        ($validate)(&frame, self.capacities()).map_err(SessionFrameError::Payload)?;
                        Ok(frame)
                    }
                    _ => Err(SessionFrameError::UnexpectedMessageKind { actual: header.kind() }),
                }
            }
        }
    };
}

fn unsupported_schema() -> SessionFrameError {
    SessionFrameError::Payload(SessionPayloadError::InvalidResponse)
}

session_codec!(encode_control_request, decode_control_request, ControlRequestV1,
    PreparationControlFrame, HostToGuest,
    |frame: &PreparationControlFrame| frame.kind().message_kind(),
    (request_id: u64, request: &PrepareRequestFrame<'_>, preparation: PreparationId),
    PreparationControlFrame::decode_for);
session_codec!(encode_control_reply, decode_control_reply, ControlReplyV1,
    ControlReplyFrame, GuestToHost,
    |frame: &ControlReplyFrame| frame.command().kind().message_kind(),
    (command: &PreparationControlFrame), ControlReplyFrame::decode_for, validate_control_reply);
session_codec!(encode_start, decode_start_frame, StartV1,
    StartFrame, HostToGuest, |_: &StartFrame| MessageKind::Start,
    (request_id: u64, request: &PrepareRequestFrame<'_>, preparation: PreparationId), StartFrame::decode_for,
    |frame: &StartFrame, capacities: &ProfileCapacities| validate_timeout(frame.request(), capacities));
session_codec!(encode_start_accepted, decode_start_accepted, StartAcceptedV1,
    StartAcceptedFrame, GuestToHost, |_: &StartAcceptedFrame| MessageKind::Start,
    (request_id: u64, request: &PrepareRequestFrame<'_>, preparation: PreparationId), StartAcceptedFrame::decode_for);
session_codec!(encode_exit, decode_exit, ExitV1,
    ExitFrame, GuestToHost, |_: &ExitFrame| MessageKind::Exit,
    (request: &PrepareRequestFrame<'_>, preparation: PreparationId), ExitFrame::decode_for);
session_codec!(encode_error, decode_error, ErrorV1,
    ErrorFrame, GuestToHost, |_: &ErrorFrame| MessageKind::Error,
    (request_header: ControlFrameHeader, request: &PrepareRequestFrame<'_>, preparation: Option<PreparationId>), ErrorFrame::decode_for);

impl ProtocolProfile {
    pub(crate) fn decode_start_payload(
        &self,
        request: &PrepareRequestFrame<'_>,
        preparation: PreparationId,
        payload: &[u8],
    ) -> Result<StartRequest, SessionPayloadError> {
        let start = match self
            .schema(MessageKind::Start, Direction::HostToGuest)
            .map(|schema| schema.codec)
        {
            Some(SchemaId::StartV1) => StartRequest::decode_for(request, preparation, payload)?,
            #[cfg(test)]
            Some(SchemaId::TestStart) => crate::profile::schema_fixture::decode_start_payload(
                self,
                request,
                preparation,
                payload,
            )?,
            _ => return Err(SessionPayloadError::InvalidResponse),
        };
        validate_timeout(&start, self.capacities())?;
        Ok(start)
    }

    pub(crate) fn encode_prepare_frame(
        &self,
        frame: &PrepareRequestFrame<'_>,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        frame
            .request()
            .validate_with(self.capacities())
            .map_err(PrepareFrameError::Payload)?;
        #[cfg(test)]
        if self
            .schema(MessageKind::PrepareRequest, Direction::HostToGuest)
            .map(|schema| schema.codec)
            == Some(SchemaId::TestPrepare)
        {
            return crate::profile::schema_fixture::encode_prepare_frame(self, frame, output);
        }
        if self
            .schema(MessageKind::PrepareRequest, Direction::HostToGuest)
            .map(|schema| schema.codec)
            != Some(SchemaId::PrepareV1)
        {
            return Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            ));
        }
        let length = frame.encode_into_with_version(self.version(), output)?;
        let header = ControlFrameHeader::decode(&output[..crate::CONTROL_FRAME_HEADER_BYTES])
            .map_err(PrepareFrameError::Header)?;
        self.validate_prepare_header(header)?;
        Ok(length)
    }

    fn check_version(
        &self,
        header: ControlFrameHeader,
    ) -> Result<(), crate::ProtocolVersionMismatch> {
        if header.protocol_version() != self.version() {
            return Err(crate::ProtocolVersionMismatch {
                expected: self.version(),
                actual: header.protocol_version(),
            });
        }
        Ok(())
    }

    fn check_capacity(&self, header: ControlFrameHeader) -> Result<(), crate::ControlLimitError> {
        let caps = self.capacities();
        let bytes = crate::CONTROL_FRAME_HEADER_BYTES as u32 + header.payload_bytes();
        if bytes > caps.max_control_frame_bytes {
            return Err(crate::ControlLimitError::FrameTooLarge {
                declared: bytes,
                maximum: caps.max_control_frame_bytes,
            });
        }
        if header.descriptor_count() > caps.max_control_descriptors {
            return Err(crate::ControlLimitError::TooManyDescriptors {
                declared: header.descriptor_count(),
                maximum: caps.max_control_descriptors,
            });
        }
        Ok(())
    }

    pub(in crate::profile) fn validate_session_header(
        &self,
        header: ControlFrameHeader,
    ) -> Result<(), SessionFrameError> {
        self.check_version(header)
            .map_err(SessionFrameError::ProtocolVersion)?;
        self.check_capacity(header)
            .map_err(SessionFrameError::Limit)
    }

    pub(in crate::profile) fn validate_prepare_header(
        &self,
        header: ControlFrameHeader,
    ) -> Result<(), PrepareFrameError> {
        self.check_version(header)
            .map_err(PrepareFrameError::ProtocolVersion)?;
        self.check_capacity(header)
            .map_err(PrepareFrameError::Limit)
    }

    pub(crate) fn decode_prepare_into<'a, 's>(
        &self,
        storage: &'s mut crate::PrepareDecodeStorage<'a>,
        header: ControlFrameHeader,
        payload: &'a [u8],
        descriptors: usize,
        broker: crate::BrokerId,
        scratch: &mut [u8],
    ) -> Result<&'s PrepareRequestFrame<'a>, PrepareFrameError> {
        storage.invalidate();
        self.validate_prepare_header(header)?;
        match self
            .schema(MessageKind::PrepareRequest, Direction::HostToGuest)
            .map(|schema| schema.codec)
        {
            #[cfg(test)]
            Some(SchemaId::TestPrepare) => crate::profile::schema_fixture::decode_prepare_into(
                self,
                storage,
                header,
                payload,
                descriptors,
                broker,
                scratch,
            ),
            Some(SchemaId::PrepareV1) => storage.decode_frame(
                header,
                payload,
                descriptors,
                broker,
                scratch,
                self.capacities(),
            ),
            _ => Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            )),
        }
    }

    pub(crate) fn encode_start_payload(
        &self,
        start: &StartRequest,
        output: &mut [u8],
    ) -> Result<usize, PayloadCodecError> {
        match self
            .schema(MessageKind::Start, Direction::HostToGuest)
            .map(|schema| schema.codec)
        {
            #[cfg(test)]
            Some(SchemaId::TestStart) => {
                crate::profile::schema_fixture::encode_start_payload(self, start, output)
            }
            Some(SchemaId::StartV1) => {
                validate_timeout(start, self.capacities())
                    .map_err(|_| PayloadCodecError::InvalidRequest)?;
                start.encode_payload(output)
            }
            _ => Err(PayloadCodecError::InvalidRequest),
        }
    }
    pub(crate) fn encode_ready(
        &self,
        frame: PrepareReadyFrame,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        match self
            .schema(MessageKind::PrepareReady, Direction::GuestToHost)
            .map(|schema| schema.codec)
        {
            Some(SchemaId::ReadyV1) => {
                self.validate_lease(frame.response().remaining_lease_ms())?;
                let length = frame.encode_into_with_version(self.version(), output)?;
                let header =
                    ControlFrameHeader::decode(&output[..crate::CONTROL_FRAME_HEADER_BYTES])
                        .map_err(PrepareFrameError::Header)?;
                self.validate_prepare_header(header)?;
                Ok(length)
            }
            _ => Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            )),
        }
    }

    pub(crate) fn decode_ready(
        &self,
        request: &PrepareRequestFrame<'_>,
        header: ControlFrameHeader,
        payload: &[u8],
        descriptors: usize,
    ) -> Result<PrepareReadyFrame, PrepareFrameError> {
        self.validate_prepare_header(header)?;
        match self
            .schema(MessageKind::PrepareReady, Direction::GuestToHost)
            .map(|schema| schema.codec)
        {
            Some(SchemaId::ReadyV1) => {
                let frame = request.decode_ready_parts(header, payload, descriptors)?;
                self.validate_lease(frame.response().remaining_lease_ms())?;
                Ok(frame)
            }
            _ => Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            )),
        }
    }

    fn validate_lease(&self, lease_ms: u32) -> Result<(), PrepareFrameError> {
        if lease_ms == 0 || lease_ms > self.capacities().max_prepared_lease_ms {
            return Err(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest,
            ));
        }
        Ok(())
    }

    pub(crate) fn encode_prepare(
        &self,
        request: &PrepareRequest<'_>,
        output: &mut [u8],
    ) -> Result<usize, PayloadCodecError> {
        match self
            .schema(MessageKind::PrepareRequest, Direction::HostToGuest)
            .map(|schema| schema.codec)
        {
            #[cfg(test)]
            Some(SchemaId::TestPrepare) => {
                crate::profile::schema_fixture::encode_prepare(self, request, output)
            }
            Some(SchemaId::PrepareV1) => {
                request.validate_with(self.capacities())?;
                request.encode_payload(output)
            }
            _ => Err(PayloadCodecError::InvalidRequest),
        }
    }

    pub(crate) fn decode_prepare<'a>(
        &self,
        payload: &'a [u8],
        scratch: &mut [u8],
    ) -> Result<PrepareRequest<'a>, PayloadCodecError> {
        match self
            .schema(MessageKind::PrepareRequest, Direction::HostToGuest)
            .map(|schema| schema.codec)
        {
            #[cfg(test)]
            Some(SchemaId::TestPrepare) => {
                crate::profile::schema_fixture::decode_prepare(self, payload, scratch)
            }
            Some(SchemaId::PrepareV1) => {
                let mut storage = crate::PrepareDecodeStorage::new();
                storage.decode(payload, scratch, self.capacities())?;
                Ok(storage.into_request())
            }
            _ => Err(PayloadCodecError::InvalidRequest),
        }
    }
}

fn validate_control_reply(
    frame: &ControlReplyFrame,
    capacities: &ProfileCapacities,
) -> Result<(), SessionPayloadError> {
    if let crate::ControlReplyStatus::Prepared { remaining_lease_ms } = frame.status() {
        if remaining_lease_ms == 0 || remaining_lease_ms > capacities.max_prepared_lease_ms {
            return Err(SessionPayloadError::InvalidResponse);
        }
    }
    Ok(())
}

pub(in crate::profile) fn validate_timeout(
    start: &StartRequest,
    capacities: &ProfileCapacities,
) -> Result<(), SessionPayloadError> {
    if start
        .execution_timeout_ms()
        .is_some_and(|timeout| timeout == 0 || timeout > capacities.max_execution_timeout_ms)
    {
        return Err(SessionPayloadError::InvalidExecutionTimeout);
    }
    Ok(())
}
