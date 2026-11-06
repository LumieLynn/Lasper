use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    MessageKind, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS, MAX_CONTROL_FRAME_BYTES,
    PROTOCOL_MAJOR,
};

const BROKER_ID_BYTES: usize = 16;
pub const HELLO_PAYLOAD_BUFFER_BYTES: usize = 256;
/// Wire-level floor: a control header fits, but no payload does.
///
/// This does not guarantee that a negotiated session can carry the helper's
/// application messages. The caller must also ensure its required messages fit.
pub const MIN_NEGOTIATED_CONTROL_FRAME_BYTES: u32 = CONTROL_FRAME_HEADER_BYTES as u32;
pub const HELLO_CONTROL_FRAME_BYTES: u32 =
    (CONTROL_FRAME_HEADER_BYTES + HELLO_PAYLOAD_BUFFER_BYTES) as u32;
/// Bootstrap envelope for the two Hello frames.
///
/// Hello cannot use negotiated limits because those limits are exchanged by
/// Hello itself. The request and response therefore use this fixed envelope;
/// the limits carried in their payloads apply only after negotiation completes.
pub const HELLO_CONTROL_LIMITS: ControlLimits = ControlLimits {
    max_control_frame_bytes: HELLO_CONTROL_FRAME_BYTES,
    max_descriptors: 0,
};

/// A per-startup identifier that prevents clients from reusing readiness from
/// an earlier broker generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BrokerId([u8; BROKER_ID_BYTES]);

impl BrokerId {
    pub const fn new(bytes: [u8; BROKER_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; BROKER_ID_BYTES] {
        &self.0
    }
}

/// Limits one peer is willing to accept after the Hello exchange.
///
/// These values do not constrain either Hello frame. Both Hello frames use
/// [`HELLO_CONTROL_LIMITS`] so that negotiation has a bounded bootstrap format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlLimits {
    max_control_frame_bytes: u32,
    max_descriptors: u16,
}

impl ControlLimits {
    pub const CURRENT: Self = Self {
        max_control_frame_bytes: MAX_CONTROL_FRAME_BYTES,
        max_descriptors: MAX_CONTROL_DESCRIPTORS,
    };

    pub const fn new(
        max_control_frame_bytes: u32,
        max_descriptors: u16,
    ) -> Result<Self, PayloadCodecError> {
        if max_control_frame_bytes < MIN_NEGOTIATED_CONTROL_FRAME_BYTES
            || max_control_frame_bytes > MAX_CONTROL_FRAME_BYTES
        {
            return Err(PayloadCodecError::InvalidLimits);
        }
        if max_descriptors > MAX_CONTROL_DESCRIPTORS {
            return Err(PayloadCodecError::InvalidLimits);
        }
        Ok(Self {
            max_control_frame_bytes,
            max_descriptors,
        })
    }

    pub const fn max_control_frame_bytes(self) -> u32 {
        self.max_control_frame_bytes
    }

    pub const fn max_descriptors(self) -> u16 {
        self.max_descriptors
    }

    pub const fn validate_header(
        self,
        header: crate::ControlFrameHeader,
    ) -> Result<(), ControlLimitError> {
        let frame_bytes = CONTROL_FRAME_HEADER_BYTES as u32 + header.payload_bytes();
        if frame_bytes > self.max_control_frame_bytes {
            return Err(ControlLimitError::FrameTooLarge {
                declared: frame_bytes,
                maximum: self.max_control_frame_bytes,
            });
        }
        if header.descriptor_count() > self.max_descriptors {
            return Err(ControlLimitError::TooManyDescriptors {
                declared: header.descriptor_count(),
                maximum: self.max_descriptors,
            });
        }
        Ok(())
    }

    const fn validate(self) -> Result<(), PayloadCodecError> {
        match Self::new(self.max_control_frame_bytes, self.max_descriptors) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    const fn intersect(self, other: Self) -> Self {
        Self {
            max_control_frame_bytes: if self.max_control_frame_bytes < other.max_control_frame_bytes
            {
                self.max_control_frame_bytes
            } else {
                other.max_control_frame_bytes
            },
            max_descriptors: if self.max_descriptors < other.max_descriptors {
                self.max_descriptors
            } else {
                other.max_descriptors
            },
        }
    }
}

/// Host capabilities and requirements sent in the first control frame.
/// Protocol major/minor are carried by the frame header, not duplicated here.
/// The advertised limits apply after the exchange; the Hello response itself
/// is bounded by [`HELLO_CONTROL_LIMITS`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    required_features: crate::FeatureSet,
    limits: ControlLimits,
}

impl HelloRequest {
    pub const KIND: MessageKind = MessageKind::HelloRequest;

    pub const fn new(
        required_features: crate::FeatureSet,
        limits: ControlLimits,
    ) -> Result<Self, PayloadCodecError> {
        match limits.validate() {
            Ok(()) => Ok(Self {
                required_features,
                limits,
            }),
            Err(error) => Err(error),
        }
    }

    pub const fn required_features(self) -> crate::FeatureSet {
        self.required_features
    }

    pub const fn limits(self) -> ControlLimits {
        self.limits
    }

    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let request: Self = decode_json_payload(bytes)?;
        request.validate()?;
        Ok(request)
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        encode_json_payload(&self, output)
    }

    fn validate(self) -> Result<(), PayloadCodecError> {
        self.limits.validate()
    }
}

/// Guest broker capabilities and generation returned by Hello.
/// Its frame is bounded by [`HELLO_CONTROL_LIMITS`], independently of either
/// peer's advertised post-Hello limits.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloResponse {
    features: crate::FeatureSet,
    broker_id: BrokerId,
    limits: ControlLimits,
}

impl HelloResponse {
    pub const KIND: MessageKind = MessageKind::HelloResponse;

    pub const fn new(
        features: crate::FeatureSet,
        broker_id: BrokerId,
        limits: ControlLimits,
    ) -> Result<Self, PayloadCodecError> {
        match limits.validate() {
            Ok(()) => Ok(Self {
                features,
                broker_id,
                limits,
            }),
            Err(error) => Err(error),
        }
    }

    pub const fn features(self) -> crate::FeatureSet {
        self.features
    }

    pub const fn broker_id(self) -> BrokerId {
        self.broker_id
    }

    pub const fn limits(self) -> ControlLimits {
        self.limits
    }

    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let response: Self = decode_json_payload(bytes)?;
        response.validate()?;
        Ok(response)
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        encode_json_payload(&self, output)
    }

    fn validate(self) -> Result<(), PayloadCodecError> {
        self.limits.validate()
    }
}

/// Validated limits that a transport can enforce immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnforcedControlLimits(ControlLimits);

impl EnforcedControlLimits {
    pub const fn get(self) -> ControlLimits {
        self.0
    }
}

impl HelloRequest {
    /// Intersects both peers' post-Hello limits.
    ///
    /// The result only describes frame capacity. It may be too small for a
    /// particular application message, which the caller must check before
    /// completing the transport handshake.
    pub fn negotiate(
        self,
        peer_protocol_major: u16,
        response: HelloResponse,
    ) -> Result<EnforcedControlLimits, HelloNegotiationError> {
        if peer_protocol_major != PROTOCOL_MAJOR {
            return Err(HelloNegotiationError::ProtocolMajor {
                expected: PROTOCOL_MAJOR,
                actual: peer_protocol_major,
            });
        }
        self.validate()
            .map_err(HelloNegotiationError::InvalidLimits)?;
        response
            .validate()
            .map_err(HelloNegotiationError::InvalidLimits)?;
        if !response.features.contains(self.required_features) {
            return Err(HelloNegotiationError::UnsupportedFeatures {
                required: self.required_features,
                available: response.features,
            });
        }

        Ok(EnforcedControlLimits(
            self.limits.intersect(response.limits),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelloNegotiationError {
    ProtocolMajor {
        expected: u16,
        actual: u16,
    },
    InvalidLimits(PayloadCodecError),
    UnsupportedFeatures {
        required: crate::FeatureSet,
        available: crate::FeatureSet,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlLimitError {
    FrameTooLarge { declared: u32, maximum: u32 },
    TooManyDescriptors { declared: u16, maximum: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlSessionError {
    Limit(ControlLimitError),
    ExpectedOutbound(MessageKind),
    ExpectedInbound(MessageKind),
    UnexpectedMessage {
        expected: MessageKind,
        actual: MessageKind,
    },
    MismatchedHelloRequestId {
        expected: u64,
        actual: u64,
    },
    NegotiationPending,
    HandshakeAlreadyComplete,
    UnexpectedHello,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlSessionState {
    phase: ControlSessionPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlSessionPhase {
    HostSendRequest,
    HostReceiveResponse { request_id: u64 },
    GuestReceiveRequest,
    GuestSendResponse { request_id: u64 },
    NegotiationPending,
    Established(ControlLimits),
}

impl ControlSessionState {
    pub const fn host() -> Self {
        Self {
            phase: ControlSessionPhase::HostSendRequest,
        }
    }

    pub const fn guest() -> Self {
        Self {
            phase: ControlSessionPhase::GuestReceiveRequest,
        }
    }

    pub fn next_outbound(
        self,
        header: crate::ControlFrameHeader,
    ) -> Result<Self, ControlSessionError> {
        match self.phase {
            ControlSessionPhase::HostSendRequest => {
                validate_hello_kind(header, MessageKind::HelloRequest)?;
                Ok(Self {
                    phase: ControlSessionPhase::HostReceiveResponse {
                        request_id: header.request_id(),
                    },
                })
            }
            ControlSessionPhase::GuestSendResponse { request_id } => {
                validate_hello_kind(header, MessageKind::HelloResponse)?;
                validate_request_id(header, request_id)?;
                Ok(Self {
                    phase: ControlSessionPhase::NegotiationPending,
                })
            }
            ControlSessionPhase::HostReceiveResponse { .. } => Err(
                ControlSessionError::ExpectedInbound(MessageKind::HelloResponse),
            ),
            ControlSessionPhase::GuestReceiveRequest => Err(ControlSessionError::ExpectedInbound(
                MessageKind::HelloRequest,
            )),
            ControlSessionPhase::NegotiationPending => Err(ControlSessionError::NegotiationPending),
            ControlSessionPhase::Established(limits) => {
                validate_established_header(limits, header)?;
                Ok(self)
            }
        }
    }

    pub fn next_inbound(
        self,
        header: crate::ControlFrameHeader,
    ) -> Result<Self, ControlSessionError> {
        match self.phase {
            ControlSessionPhase::GuestReceiveRequest => {
                validate_hello_kind(header, MessageKind::HelloRequest)?;
                Ok(Self {
                    phase: ControlSessionPhase::GuestSendResponse {
                        request_id: header.request_id(),
                    },
                })
            }
            ControlSessionPhase::HostReceiveResponse { request_id } => {
                validate_hello_kind(header, MessageKind::HelloResponse)?;
                validate_request_id(header, request_id)?;
                Ok(Self {
                    phase: ControlSessionPhase::NegotiationPending,
                })
            }
            ControlSessionPhase::HostSendRequest => Err(ControlSessionError::ExpectedOutbound(
                MessageKind::HelloRequest,
            )),
            ControlSessionPhase::GuestSendResponse { .. } => Err(
                ControlSessionError::ExpectedOutbound(MessageKind::HelloResponse),
            ),
            ControlSessionPhase::NegotiationPending => Err(ControlSessionError::NegotiationPending),
            ControlSessionPhase::Established(limits) => {
                validate_established_header(limits, header)?;
                Ok(self)
            }
        }
    }

    /// Starts enforcing the limits negotiated from this session's Hello pair.
    ///
    /// The transport checks that the role-specific pair was exchanged and that
    /// its request IDs match. It cannot associate this opaque limits value with
    /// that exchange, so the caller must pass the result of negotiating that
    /// exact request and response. It must also ensure the result can carry the
    /// application messages required by this session.
    pub fn complete_hello(
        &mut self,
        limits: EnforcedControlLimits,
    ) -> Result<(), ControlSessionError> {
        match self.phase {
            ControlSessionPhase::NegotiationPending => {
                self.phase = ControlSessionPhase::Established(limits.get());
                Ok(())
            }
            ControlSessionPhase::Established(_) => {
                Err(ControlSessionError::HandshakeAlreadyComplete)
            }
            ControlSessionPhase::HostSendRequest
            | ControlSessionPhase::HostReceiveResponse { .. }
            | ControlSessionPhase::GuestReceiveRequest
            | ControlSessionPhase::GuestSendResponse { .. } => {
                Err(ControlSessionError::NegotiationPending)
            }
        }
    }
}

fn validate_hello_kind(
    header: crate::ControlFrameHeader,
    expected: MessageKind,
) -> Result<(), ControlSessionError> {
    if header.kind() != expected {
        return Err(ControlSessionError::UnexpectedMessage {
            expected,
            actual: header.kind(),
        });
    }
    match HELLO_CONTROL_LIMITS.validate_header(header) {
        Ok(()) => Ok(()),
        Err(error) => Err(ControlSessionError::Limit(error)),
    }
}

fn validate_request_id(
    header: crate::ControlFrameHeader,
    expected: u64,
) -> Result<(), ControlSessionError> {
    if header.request_id() == expected {
        Ok(())
    } else {
        Err(ControlSessionError::MismatchedHelloRequestId {
            expected,
            actual: header.request_id(),
        })
    }
}

fn validate_established_header(
    limits: ControlLimits,
    header: crate::ControlFrameHeader,
) -> Result<(), ControlSessionError> {
    if matches!(
        header.kind(),
        MessageKind::HelloRequest | MessageKind::HelloResponse
    ) {
        return Err(ControlSessionError::UnexpectedHello);
    }
    match limits.validate_header(header) {
        Ok(()) => Ok(()),
        Err(error) => Err(ControlSessionError::Limit(error)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadCodecError {
    PayloadTooLarge,
    OutputTooSmall,
    InvalidJson,
    InvalidLimits,
    InvalidRequest,
}

/// Serializes a control payload into caller-owned storage without allocating.
pub(crate) fn encode_json_payload<T: Serialize + ?Sized>(
    value: &T,
    output: &mut [u8],
) -> Result<usize, PayloadCodecError> {
    let protocol_capacity = crate::MAX_CONTROL_PAYLOAD_BYTES as usize;
    let capacity = output.len().min(protocol_capacity);
    serde_json_core::to_slice(value, &mut output[..capacity]).map_err(|_| {
        if output.len() >= protocol_capacity {
            PayloadCodecError::PayloadTooLarge
        } else {
            PayloadCodecError::OutputTooSmall
        }
    })
}

/// Deserializes one bounded JSON payload.
pub(crate) fn decode_json_payload<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, PayloadCodecError> {
    if bytes.len() > crate::MAX_CONTROL_PAYLOAD_BYTES as usize {
        return Err(PayloadCodecError::PayloadTooLarge);
    }
    serde_json_core::from_slice(bytes)
        .map(|(value, _consumed)| value)
        .map_err(|_| PayloadCodecError::InvalidJson)
}

/// Deserializes a bounded JSON payload, using caller storage to unescape strings.
pub(crate) fn decode_json_payload_escaped<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    unescape_buffer: &mut [u8],
) -> Result<T, PayloadCodecError> {
    if bytes.len() > crate::MAX_CONTROL_PAYLOAD_BYTES as usize {
        return Err(PayloadCodecError::PayloadTooLarge);
    }
    serde_json_core::from_slice_escaped(bytes, unescape_buffer)
        .map(|(value, _consumed)| value)
        .map_err(|_| PayloadCodecError::InvalidJson)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::FeatureSet;

    #[test]
    fn hello_request_round_trips_as_bounded_json() {
        let request =
            HelloRequest::new(FeatureSet::from_bits(0b101), ControlLimits::CURRENT).unwrap();
        let mut payload = [0; 256];

        let length = request.encode_payload(&mut payload).unwrap();
        assert_eq!(
            &payload[..length],
            br#"{"required_features":5,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":3}}"#
        );
        let decoded = HelloRequest::decode_payload(&payload[..length]).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn hello_response_round_trips_broker_generation_and_limits() {
        let response = HelloResponse::new(
            FeatureSet::from_bits(0b111),
            BrokerId::new(*b"broker-generatio"),
            ControlLimits::CURRENT,
        )
        .unwrap();
        let mut payload = [0; 256];

        let length = response.encode_payload(&mut payload).unwrap();
        let decoded = HelloResponse::decode_payload(&payload[..length]).unwrap();

        assert_eq!(decoded, response);
    }

    #[test]
    fn hello_buffer_fits_the_largest_current_response() {
        let response = HelloResponse::new(
            FeatureSet::from_bits(u64::MAX),
            BrokerId::new([u8::MAX; BROKER_ID_BYTES]),
            ControlLimits::CURRENT,
        )
        .unwrap();
        let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

        assert!(response.encode_payload(&mut payload).is_ok());
    }

    #[test]
    fn hello_buffer_fits_the_largest_current_request() {
        let request =
            HelloRequest::new(FeatureSet::from_bits(u64::MAX), ControlLimits::CURRENT).unwrap();
        let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];

        assert!(request.encode_payload(&mut payload).is_ok());
    }

    #[test]
    fn decoder_accepts_json_whitespace_but_rejects_trailing_values() {
        let payload = br#"{"required_features":0,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}}  "#;
        assert!(HelloRequest::decode_payload(payload).is_ok());

        let mut trailing = payload.to_vec();
        trailing.extend_from_slice(br#"{"extra":true}"#);
        assert_eq!(
            HelloRequest::decode_payload(&trailing),
            Err(PayloadCodecError::InvalidJson)
        );
    }

    #[test]
    fn hello_rejects_duplicate_unknown_and_malformed_fields() {
        let duplicate = br#"{"required_features":0,"required_features":1,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}}"#;
        let unknown = br#"{"required_features":0,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2},"extra":true}"#;
        let malformed = br#"{"required_features":-1,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":2}}"#;

        assert_eq!(
            HelloRequest::decode_payload(duplicate),
            Err(PayloadCodecError::InvalidJson)
        );
        assert_eq!(
            HelloRequest::decode_payload(unknown),
            Err(PayloadCodecError::InvalidJson)
        );
        assert_eq!(
            HelloRequest::decode_payload(malformed),
            Err(PayloadCodecError::InvalidJson)
        );
    }

    #[test]
    fn control_limits_are_validated_after_deserialization() {
        let too_large = br#"{"required_features":0,"limits":{"max_control_frame_bytes":1048577,"max_descriptors":2}}"#;
        let too_many_fds = br#"{"required_features":0,"limits":{"max_control_frame_bytes":1048576,"max_descriptors":4}}"#;
        let too_small = br#"{"required_features":0,"limits":{"max_control_frame_bytes":31,"max_descriptors":0}}"#;

        for payload in [&too_large[..], &too_many_fds[..], &too_small[..]] {
            assert_eq!(
                HelloRequest::decode_payload(payload),
                Err(PayloadCodecError::InvalidLimits)
            );
        }
    }

    #[test]
    fn negotiation_intersects_limits_and_rejects_missing_features() {
        let request = HelloRequest::new(
            FeatureSet::from_bits(0b011),
            ControlLimits::new(65536, 2).unwrap(),
        )
        .unwrap();
        let response = HelloResponse::new(
            FeatureSet::from_bits(0b111),
            BrokerId::new([1; BROKER_ID_BYTES]),
            ControlLimits::new(32768, 1).unwrap(),
        )
        .unwrap();

        assert_eq!(
            request
                .negotiate(crate::PROTOCOL_MAJOR, response)
                .unwrap()
                .get(),
            ControlLimits::new(32768, 1).unwrap()
        );

        let unsupported =
            HelloRequest::new(FeatureSet::from_bits(0b1000), ControlLimits::CURRENT).unwrap();
        assert_eq!(
            unsupported.negotiate(crate::PROTOCOL_MAJOR, response),
            Err(HelloNegotiationError::UnsupportedFeatures {
                required: FeatureSet::from_bits(0b1000),
                available: FeatureSet::from_bits(0b111),
            })
        );

        assert_eq!(
            request.negotiate(crate::PROTOCOL_MAJOR + 1, response),
            Err(HelloNegotiationError::ProtocolMajor {
                expected: crate::PROTOCOL_MAJOR,
                actual: crate::PROTOCOL_MAJOR + 1,
            })
        );
    }

    #[test]
    fn codec_enforces_payload_and_output_bounds() {
        let response = HelloResponse::new(
            FeatureSet::EMPTY,
            BrokerId::new([0; BROKER_ID_BYTES]),
            ControlLimits::CURRENT,
        )
        .unwrap();
        let mut short = [0; 8];
        assert_eq!(
            encode_json_payload(&response, &mut short),
            Err(PayloadCodecError::OutputTooSmall)
        );

        let oversized_value = std::string::String::from_iter(std::iter::repeat_n(
            'x',
            crate::MAX_CONTROL_PAYLOAD_BYTES as usize,
        ));
        let mut maximum_output = std::vec![0; crate::MAX_CONTROL_PAYLOAD_BYTES as usize + 1024];
        assert_eq!(
            encode_json_payload(oversized_value.as_str(), &mut maximum_output),
            Err(PayloadCodecError::PayloadTooLarge)
        );

        let oversized = [b' '; crate::MAX_CONTROL_PAYLOAD_BYTES as usize + 1];
        assert_eq!(
            HelloRequest::decode_payload(&oversized),
            Err(PayloadCodecError::PayloadTooLarge)
        );
    }

    #[test]
    fn negotiated_limits_reject_frames_and_descriptors_beyond_the_peer_offer() {
        let frame_limit = CONTROL_FRAME_HEADER_BYTES as u32 + 8;
        let limits = ControlLimits::new(frame_limit, 0).unwrap();
        let oversized = crate::ControlFrameHeader::new(
            MessageKind::PrepareRequest,
            1,
            frame_limit - CONTROL_FRAME_HEADER_BYTES as u32 + 1,
            0,
        )
        .unwrap();
        let descriptor =
            crate::ControlFrameHeader::new(MessageKind::PrepareReady, 1, 0, 1).unwrap();

        assert_eq!(
            limits.validate_header(oversized),
            Err(ControlLimitError::FrameTooLarge {
                declared: frame_limit + 1,
                maximum: frame_limit,
            })
        );
        assert_eq!(
            limits.validate_header(descriptor),
            Err(ControlLimitError::TooManyDescriptors {
                declared: 1,
                maximum: 0,
            })
        );
    }

    #[test]
    fn control_session_requires_exactly_one_bounded_hello() {
        let mut host = ControlSessionState::host();
        let mut guest = ControlSessionState::guest();
        let prepare = crate::ControlFrameHeader::new(MessageKind::PrepareRequest, 1, 0, 0).unwrap();
        assert_eq!(
            host.next_outbound(prepare),
            Err(ControlSessionError::UnexpectedMessage {
                expected: MessageKind::HelloRequest,
                actual: MessageKind::PrepareRequest,
            })
        );
        assert_eq!(
            host.next_inbound(prepare),
            Err(ControlSessionError::ExpectedOutbound(
                MessageKind::HelloRequest
            ))
        );
        assert_eq!(
            guest.next_outbound(prepare),
            Err(ControlSessionError::ExpectedInbound(
                MessageKind::HelloRequest
            ))
        );

        let oversized_request = crate::ControlFrameHeader::new(
            MessageKind::HelloRequest,
            17,
            HELLO_PAYLOAD_BUFFER_BYTES as u32 + 1,
            0,
        )
        .unwrap();
        assert!(matches!(
            host.next_outbound(oversized_request),
            Err(ControlSessionError::Limit(
                ControlLimitError::FrameTooLarge { .. }
            ))
        ));

        let request_header =
            crate::ControlFrameHeader::new(MessageKind::HelloRequest, 17, 0, 0).unwrap();
        host = host.next_outbound(request_header).unwrap();
        guest = guest.next_inbound(request_header).unwrap();
        assert_eq!(
            host.next_outbound(request_header),
            Err(ControlSessionError::ExpectedInbound(
                MessageKind::HelloResponse
            ))
        );
        assert_eq!(
            guest.next_inbound(request_header),
            Err(ControlSessionError::ExpectedOutbound(
                MessageKind::HelloResponse
            ))
        );

        let response_header =
            crate::ControlFrameHeader::new(MessageKind::HelloResponse, 17, 0, 0).unwrap();
        guest = guest.next_outbound(response_header).unwrap();
        host = host.next_inbound(response_header).unwrap();
        assert_eq!(
            host.next_inbound(response_header),
            Err(ControlSessionError::NegotiationPending)
        );
        assert_eq!(
            guest.next_outbound(response_header),
            Err(ControlSessionError::NegotiationPending)
        );

        let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
        let response = HelloResponse::new(
            FeatureSet::EMPTY,
            BrokerId::new([3; BROKER_ID_BYTES]),
            ControlLimits::CURRENT,
        )
        .unwrap();
        let established = request.negotiate(crate::PROTOCOL_MAJOR, response).unwrap();
        host.complete_hello(established).unwrap();
        guest.complete_hello(established).unwrap();

        let repeated_hello =
            crate::ControlFrameHeader::new(MessageKind::HelloResponse, 1, 0, 0).unwrap();
        assert_eq!(
            host.next_inbound(repeated_hello),
            Err(ControlSessionError::UnexpectedHello)
        );
        assert_eq!(
            host.complete_hello(established),
            Err(ControlSessionError::HandshakeAlreadyComplete)
        );
        assert_eq!(host.next_outbound(prepare), Ok(host));
        assert_eq!(guest.next_inbound(prepare), Ok(guest));
    }

    #[test]
    fn hello_response_must_match_the_request_id() {
        let request = crate::ControlFrameHeader::new(MessageKind::HelloRequest, 41, 0, 0).unwrap();
        let response =
            crate::ControlFrameHeader::new(MessageKind::HelloResponse, 42, 0, 0).unwrap();
        let host = ControlSessionState::host().next_outbound(request).unwrap();
        let guest = ControlSessionState::guest().next_inbound(request).unwrap();

        let expected = Err(ControlSessionError::MismatchedHelloRequestId {
            expected: 41,
            actual: 42,
        });
        assert_eq!(host.next_inbound(response), expected);
        assert_eq!(guest.next_outbound(response), expected);
    }
}
