//! Peer capacity declarations and the effective limits installed after Hello.

use crate::{
    ControlFrameHeader, PayloadCodecError, CONTROL_FRAME_HEADER_BYTES, MAX_CONTROL_DESCRIPTORS,
    MAX_CONTROL_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};

pub const HELLO_PAYLOAD_BUFFER_BYTES: usize = super::BOOTSTRAP_MANIFEST.payload_bytes();
/// Smallest capacity declaration that can hold a header. Negotiation also
/// requires the selected profile's minimum frame capacity.
pub const MIN_NEGOTIATED_CONTROL_FRAME_BYTES: u32 = CONTROL_FRAME_HEADER_BYTES as u32;
pub const HELLO_CONTROL_FRAME_BYTES: u32 =
    super::BOOTSTRAP_MANIFEST.limits.max_control_frame_bytes();
/// Fixed envelope for HelloRequest, HelloResponse and HelloRejected.
///
/// Hello cannot use negotiated limits because those limits are exchanged by
/// Hello itself. All three messages therefore use this fixed envelope;
/// the limits carried in their payloads apply only after negotiation completes.
pub const HELLO_CONTROL_LIMITS: ControlLimits = super::BOOTSTRAP_MANIFEST.limits;

/// Limits one peer is willing to accept after the Hello exchange.
///
/// These values do not constrain Hello messages. All Hello messages use
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

    /// A capacity declaration may exceed this build's storage bounds. Hello
    /// intersects both declarations with the selected profile before installing
    /// enforceable limits; receiving an offer does not allocate that capacity.
    pub const fn new(
        max_control_frame_bytes: u32,
        max_descriptors: u16,
    ) -> Result<Self, PayloadCodecError> {
        if max_control_frame_bytes < MIN_NEGOTIATED_CONTROL_FRAME_BYTES {
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
        header: ControlFrameHeader,
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

    pub(crate) const fn validate(self) -> Result<(), PayloadCodecError> {
        match Self::new(self.max_control_frame_bytes, self.max_descriptors) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) const fn intersect(self, other: Self) -> Self {
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

/// Validated limits that a transport can enforce immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnforcedControlLimits(ControlLimits);

impl EnforcedControlLimits {
    pub(crate) const fn new(limits: ControlLimits) -> Self {
        Self(limits)
    }

    pub const fn get(self) -> ControlLimits {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlLimitError {
    FrameTooLarge { declared: u32, maximum: u32 },
    TooManyDescriptors { declared: u16, maximum: u16 },
}

#[cfg(test)]
#[path = "../../tests/messages/limits.rs"]
mod tests;
