//! Bootstrap offers and responses. A decoded response becomes an agreed
//! protocol only after validation against the host's original request.

use super::bootstrap::{self, BootstrapPayload, BootstrapSchemaId};
use crate::{ControlLimits, MessageKind, PayloadCodecError, ProtocolVersion};
use serde::{Deserialize, Serialize};

const BROKER_ID_BYTES: usize = 16;

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

/// Host offer and the capabilities required for this connection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    offer: crate::HelloOffer,
    required_features: crate::FeatureSet,
}

impl HelloRequest {
    pub const KIND: MessageKind = MessageKind::HelloRequest;

    pub const fn new(
        required_features: crate::FeatureSet,
        limits: ControlLimits,
    ) -> Result<Self, PayloadCodecError> {
        Self::from_offer(crate::HelloOffer::local(limits), required_features)
    }

    pub const fn from_offer(
        offer: crate::HelloOffer,
        required_features: crate::FeatureSet,
    ) -> Result<Self, PayloadCodecError> {
        if !offer.supported_features.contains(required_features) {
            return Err(PayloadCodecError::InvalidRequest);
        }
        match offer.validate() {
            Ok(()) => Ok(Self {
                offer,
                required_features,
            }),
            Err(error) => Err(error),
        }
    }

    pub const fn offer(self) -> crate::HelloOffer {
        self.offer
    }

    pub const fn required_features(self) -> crate::FeatureSet {
        self.required_features
    }

    pub const fn limits(self) -> ControlLimits {
        self.offer.limits
    }

    /// Decodes the bootstrap schema. Offer semantics are checked by negotiation
    /// so the guest can return HelloRejected(InvalidOffer) to an invalid offer.
    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        bootstrap::decode(bytes)
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        Self::from_offer(self.offer, self.required_features)?;
        bootstrap::encode(&self, output)
    }

    /// Recomputes the selection from this original request and checks both the
    /// peer's bootstrap version and its claimed selection. The returned protocol
    /// can then be installed in the connection state.
    pub fn negotiate_protocol(
        self,
        peer_protocol_version: ProtocolVersion,
        response: HelloResponse,
    ) -> Result<crate::NegotiatedProtocol, crate::CompatibilityError> {
        self.negotiate_protocol_with_profiles(
            peer_protocol_version,
            response,
            crate::protocol_profiles(),
        )
    }

    pub(crate) fn negotiate_protocol_with_profiles(
        self,
        peer_protocol_version: ProtocolVersion,
        response: HelloResponse,
        profiles: &'static [crate::ProtocolProfile],
    ) -> Result<crate::NegotiatedProtocol, crate::CompatibilityError> {
        if peer_protocol_version.major != response.offer.protocols.major
            || peer_protocol_version.minor != crate::BOOTSTRAP_MANIFEST.minor
        {
            return Err(crate::CompatibilityError::InvalidSelection);
        }
        let negotiated = crate::profile::negotiate_with(self, response.offer, profiles)?;
        if response.selected != negotiated.selection() {
            return Err(crate::CompatibilityError::InvalidSelection);
        }
        Ok(negotiated)
    }
}

/// The guest offer and the selection computed from the original host request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloResponse {
    offer: crate::HelloOffer,
    selected: crate::HelloSelection,
    broker_id: BrokerId,
}

impl HelloResponse {
    pub const KIND: MessageKind = MessageKind::HelloResponse;

    pub fn accept(
        request: HelloRequest,
        offer: crate::HelloOffer,
        broker_id: BrokerId,
    ) -> Result<(Self, crate::NegotiatedProtocol), crate::CompatibilityError> {
        Self::accept_with_profiles(request, offer, broker_id, crate::protocol_profiles())
    }

    pub(crate) fn accept_with_profiles(
        request: HelloRequest,
        offer: crate::HelloOffer,
        broker_id: BrokerId,
        profiles: &'static [crate::ProtocolProfile],
    ) -> Result<(Self, crate::NegotiatedProtocol), crate::CompatibilityError> {
        let negotiated = crate::profile::negotiate_with(request, offer, profiles)?;
        Ok((
            Self {
                offer,
                selected: negotiated.selection(),
                broker_id,
            },
            negotiated,
        ))
    }

    pub const fn offer(self) -> crate::HelloOffer {
        self.offer
    }

    pub const fn selected(self) -> crate::HelloSelection {
        self.selected
    }

    pub const fn features(self) -> crate::FeatureSet {
        self.selected.features
    }

    pub const fn broker_id(self) -> BrokerId {
        self.broker_id
    }

    pub const fn limits(self) -> ControlLimits {
        self.selected.limits
    }

    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let response: Self = bootstrap::decode(bytes)?;
        response.offer.validate()?;
        response.selected.limits.validate()?;
        Ok(response)
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.offer.validate()?;
        self.selected.limits.validate()?;
        bootstrap::encode(&self, output)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRejected {
    pub offer: crate::HelloOffer,
    pub reason: crate::CompatibilityError,
}

impl HelloRejected {
    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.offer.validate()?;
        if self.reason == crate::CompatibilityError::InvalidSelection {
            return Err(PayloadCodecError::InvalidRequest);
        }
        bootstrap::encode(&self, output)
    }

    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let value: Self = bootstrap::decode(bytes)?;
        value.offer.validate()?;
        if value.reason == crate::CompatibilityError::InvalidSelection {
            return Err(PayloadCodecError::InvalidRequest);
        }
        if value.reason == crate::CompatibilityError::InvalidOffer {
            crate::json::require_top_level_strings(bytes, &["reason"])?;
        }
        Ok(value)
    }

    /// Checks the guest's reason against the original host offer. A rejection
    /// carries a valid guest offer even when the host's offer was invalid.
    pub fn validate_for(
        self,
        request: HelloRequest,
        peer_version: ProtocolVersion,
    ) -> Result<(), crate::CompatibilityError> {
        if self.offer.validate().is_err()
            || peer_version.major != self.offer.protocols.major
            || peer_version.minor != crate::BOOTSTRAP_MANIFEST.minor
            || crate::negotiate_hello(request, self.offer).err() != Some(self.reason)
        {
            return Err(crate::CompatibilityError::InvalidSelection);
        }
        Ok(())
    }
}

impl BootstrapPayload for HelloRequest {
    const KIND: MessageKind = MessageKind::HelloRequest;
    const CODEC: BootstrapSchemaId = BootstrapSchemaId::HelloRequestV1;
}

impl BootstrapPayload for HelloResponse {
    const KIND: MessageKind = MessageKind::HelloResponse;
    const CODEC: BootstrapSchemaId = BootstrapSchemaId::HelloResponseV1;
}

impl BootstrapPayload for HelloRejected {
    const KIND: MessageKind = MessageKind::HelloRejected;
    const CODEC: BootstrapSchemaId = BootstrapSchemaId::HelloRejectedV1;
}

#[cfg(test)]
#[path = "../../tests/messages/hello.rs"]
mod tests;
