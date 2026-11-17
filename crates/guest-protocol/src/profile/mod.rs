//! The manifest defines supported schemas, capacities and feature gates.
//! Hello selects a registered profile and narrows its features and transport
//! limits for one connection.

#[cfg(test)]
#[macro_use]
#[path = "../../tests/profile/schema_dispatch.rs"]
mod schema_dispatch;
mod codec;
#[cfg(test)]
#[path = "../../tests/profile/decoding.rs"]
mod decoding_tests;
#[cfg(test)]
#[path = "../../tests/profile/dispatch.rs"]
mod dispatch_tests;
#[cfg(test)]
#[path = "../../tests/profile/gates.rs"]
mod gate_tests;
mod manifest;
#[cfg(test)]
#[path = "../../tests/profile/schema.rs"]
mod schema_fixture;
pub use manifest::*;

use crate::{ControlLimits, EnforcedControlLimits, FeatureSet, HelloRequest, ProtocolVersion};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
}

impl ProtocolRange {
    pub const CURRENT: Self = HelloOffer::local(ControlLimits::CURRENT).protocols;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloOffer {
    pub protocols: ProtocolRange,
    pub supported_features: FeatureSet,
    pub limits: ControlLimits,
}

impl HelloOffer {
    pub(crate) const fn validate(self) -> Result<(), crate::PayloadCodecError> {
        if self.protocols.min_minor > self.protocols.max_minor {
            return Err(crate::PayloadCodecError::InvalidRequest);
        }
        self.limits.validate()
    }

    /// Advertises the codecs and features registered in this build's manifest.
    /// Call after checking the runtime prerequisites of those features.
    pub const fn local(limits: ControlLimits) -> Self {
        offer_from_profiles(protocol_profiles(), limits)
    }
}

const fn offer_from_profiles(profiles: &[ProtocolProfile], limits: ControlLimits) -> HelloOffer {
    assert!(
        !profiles.is_empty(),
        "the protocol manifest must not be empty"
    );
    let mut protocols = ProtocolRange {
        major: profiles[0].version().major,
        min_minor: profiles[0].version().minor,
        max_minor: profiles[0].version().minor,
    };
    let mut features = FeatureSet::EMPTY;
    let mut frame_bytes = 0;
    let mut descriptors = 0;
    let mut index = 0;
    while index < profiles.len() {
        let profile = &profiles[index];
        assert!(
            profile.version().major == protocols.major,
            "one offer supports one major"
        );
        let minor = profile.version().minor;
        if minor < protocols.min_minor {
            protocols.min_minor = minor;
        }
        if minor > protocols.max_minor {
            protocols.max_minor = minor;
        }
        features = features.union(profile.features());
        let capacities = profile.capacities();
        if capacities.max_control_frame_bytes > frame_bytes {
            frame_bytes = capacities.max_control_frame_bytes;
        }
        if capacities.max_control_descriptors > descriptors {
            descriptors = capacities.max_control_descriptors;
        }
        index += 1;
    }
    let mut minor = protocols.min_minor;
    loop {
        let mut matches = 0;
        let mut index = 0;
        while index < profiles.len() {
            if profiles[index].version().minor == minor {
                matches += 1;
            }
            index += 1;
        }
        assert!(matches == 1, "profile minors must be unique and continuous");
        if minor == protocols.max_minor {
            break;
        }
        minor += 1;
    }
    let compiled_limits = match ControlLimits::new(frame_bytes, descriptors) {
        Ok(value) => value,
        Err(_) => panic!("profile frame capacity cannot hold a control header"),
    };
    HelloOffer {
        protocols,
        supported_features: features,
        limits: limits.intersect(compiled_limits),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloSelection {
    pub version: ProtocolVersion,
    pub features: FeatureSet,
    pub limits: ControlLimits,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum CompatibilityError {
    MajorMismatch {
        local: u16,
        peer: u16,
    },
    NoCommonMinor {
        local: ProtocolRange,
        peer: ProtocolRange,
    },
    MissingFeatures {
        required: FeatureSet,
        available: FeatureSet,
    },
    InsufficientFrameCapacity {
        required: u32,
        available: u32,
    },
    InvalidOffer,
    InvalidSelection,
}

/// The selected profile together with the features and limits agreed in Hello.
/// Its codecs check schema, capacities and enabled features. Transports must
/// send encoded bytes only after the codec succeeds, and use WireSessionState
/// to check the order of validated messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedProtocol {
    profile: &'static ProtocolProfile,
    features: FeatureSet,
    limits: EnforcedControlLimits,
}

impl NegotiatedProtocol {
    pub const fn profile(&self) -> &'static ProtocolProfile {
        self.profile
    }
    pub const fn features(&self) -> FeatureSet {
        self.features
    }
    pub const fn limits(&self) -> EnforcedControlLimits {
        self.limits
    }
    pub const fn selection(&self) -> HelloSelection {
        HelloSelection {
            version: self.profile.version(),
            features: self.features,
            limits: self.limits.get(),
        }
    }
}

pub fn negotiate_hello(
    request: HelloRequest,
    guest: HelloOffer,
) -> Result<NegotiatedProtocol, CompatibilityError> {
    negotiate_with(request, guest, protocol_profiles())
}

pub(crate) fn negotiate_with(
    request: HelloRequest,
    guest: HelloOffer,
    profiles: &'static [ProtocolProfile],
) -> Result<NegotiatedProtocol, CompatibilityError> {
    let host = request.offer();
    for offer in [host, guest] {
        if offer.validate().is_err() {
            return Err(CompatibilityError::InvalidOffer);
        }
    }
    if !host
        .supported_features
        .contains(request.required_features())
    {
        return Err(CompatibilityError::InvalidOffer);
    }
    if host.protocols.major != guest.protocols.major {
        return Err(CompatibilityError::MajorMismatch {
            local: guest.protocols.major,
            peer: host.protocols.major,
        });
    }
    let low = host.protocols.min_minor.max(guest.protocols.min_minor);
    let high = host.protocols.max_minor.min(guest.protocols.max_minor);
    if low > high {
        return Err(CompatibilityError::NoCommonMinor {
            local: guest.protocols,
            peer: host.protocols,
        });
    }
    let profile = profiles
        .iter()
        .find(|p| p.version().major == host.protocols.major && p.version().minor == high)
        .ok_or(CompatibilityError::InvalidOffer)?;
    let features = host
        .supported_features
        .intersection(guest.supported_features)
        .intersection(profile.features());
    let required = request
        .required_features()
        .union(profile.required_features());
    if !features.contains(required) {
        return Err(CompatibilityError::MissingFeatures {
            required,
            available: features,
        });
    }
    let caps = profile.capacities();
    let limits = host.limits.intersect(guest.limits).intersect(
        ControlLimits::new(caps.max_control_frame_bytes, caps.max_control_descriptors)
            .map_err(|_| CompatibilityError::InvalidOffer)?,
    );
    if limits.max_control_frame_bytes() < caps.min_control_frame_bytes {
        return Err(CompatibilityError::InsufficientFrameCapacity {
            required: caps.min_control_frame_bytes,
            available: limits.max_control_frame_bytes(),
        });
    }
    Ok(NegotiatedProtocol {
        profile,
        features,
        limits: EnforcedControlLimits::new(limits),
    })
}

#[cfg(test)]
#[path = "../../tests/profile/negotiation.rs"]
mod tests;
