use crate::{MAX_CONTROL_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR};

const IDENTITY_MAGIC: [u8; 8] = *b"LSPRGRT1";
const IDENTITY_FORMAT: u16 = 1;
pub const IDENTITY_FRAME_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Architecture {
    X86_64 = 1,
    Aarch64 = 2,
}

impl Architecture {
    fn from_wire(value: u16) -> Result<Self, IdentityDecodeError> {
        match value {
            1 => Ok(Self::X86_64),
            2 => Ok(Self::Aarch64),
            _ => Err(IdentityDecodeError::UnknownArchitecture),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureSet(u64);

impl FeatureSet {
    pub const EMPTY: Self = Self(0);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeIdentity {
    pub architecture: Architecture,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub features: FeatureSet,
    pub max_control_frame_bytes: u32,
}

impl RuntimeIdentity {
    pub const fn current(architecture: Architecture) -> Self {
        Self {
            architecture,
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            features: FeatureSet::EMPTY,
            max_control_frame_bytes: MAX_CONTROL_FRAME_BYTES,
        }
    }

    pub const fn encode(self) -> [u8; IDENTITY_FRAME_BYTES] {
        let mut frame = [0; IDENTITY_FRAME_BYTES];
        let format = IDENTITY_FORMAT.to_le_bytes();
        let major = self.protocol_major.to_le_bytes();
        let minor = self.protocol_minor.to_le_bytes();
        let architecture = (self.architecture as u16).to_le_bytes();
        let features = self.features.bits().to_le_bytes();
        let max_frame = self.max_control_frame_bytes.to_le_bytes();

        let mut index = 0;
        while index < IDENTITY_MAGIC.len() {
            frame[index] = IDENTITY_MAGIC[index];
            index += 1;
        }
        frame[8] = format[0];
        frame[9] = format[1];
        frame[10] = major[0];
        frame[11] = major[1];
        frame[12] = minor[0];
        frame[13] = minor[1];
        frame[14] = architecture[0];
        frame[15] = architecture[1];

        index = 0;
        while index < features.len() {
            frame[16 + index] = features[index];
            index += 1;
        }
        index = 0;
        while index < max_frame.len() {
            frame[24 + index] = max_frame[index];
            index += 1;
        }
        frame
    }

    pub fn decode(frame: &[u8]) -> Result<Self, IdentityDecodeError> {
        if frame.len() != IDENTITY_FRAME_BYTES {
            return Err(IdentityDecodeError::WrongLength);
        }
        if frame[..8] != IDENTITY_MAGIC {
            return Err(IdentityDecodeError::WrongMagic);
        }
        if read_u16(frame, 8) != IDENTITY_FORMAT {
            return Err(IdentityDecodeError::UnsupportedFormat);
        }
        if frame[28..] != [0; 4] {
            return Err(IdentityDecodeError::ReservedBytesSet);
        }

        Ok(Self {
            protocol_major: read_u16(frame, 10),
            protocol_minor: read_u16(frame, 12),
            architecture: Architecture::from_wire(read_u16(frame, 14))?,
            features: FeatureSet::from_bits(read_u64(frame, 16)),
            max_control_frame_bytes: read_u32(frame, 24),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRequirements {
    pub architecture: Architecture,
    pub required_features: FeatureSet,
    pub minimum_control_frame_bytes: u32,
}

impl RuntimeRequirements {
    pub fn check(self, identity: RuntimeIdentity) -> Result<(), RuntimeCompatibilityError> {
        if identity.protocol_major != PROTOCOL_MAJOR {
            return Err(RuntimeCompatibilityError::ProtocolMajor {
                expected: PROTOCOL_MAJOR,
                actual: identity.protocol_major,
            });
        }
        if identity.architecture != self.architecture {
            return Err(RuntimeCompatibilityError::Architecture {
                expected: self.architecture,
                actual: identity.architecture,
            });
        }
        if !identity.features.contains(self.required_features) {
            return Err(RuntimeCompatibilityError::Features {
                required: self.required_features,
                available: identity.features,
            });
        }
        if identity.max_control_frame_bytes < self.minimum_control_frame_bytes {
            return Err(RuntimeCompatibilityError::ControlFrameLimit {
                required: self.minimum_control_frame_bytes,
                available: identity.max_control_frame_bytes,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCompatibilityError {
    ProtocolMajor {
        expected: u16,
        actual: u16,
    },
    Architecture {
        expected: Architecture,
        actual: Architecture,
    },
    Features {
        required: FeatureSet,
        available: FeatureSet,
    },
    ControlFrameLimit {
        required: u32,
        available: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityDecodeError {
    WrongLength,
    WrongMagic,
    UnsupportedFormat,
    UnknownArchitecture,
    ReservedBytesSet,
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IDENTITY: RuntimeIdentity = RuntimeIdentity::current(Architecture::X86_64);

    #[test]
    fn current_identity_round_trips() {
        let encoded = TEST_IDENTITY.encode();

        assert_eq!(RuntimeIdentity::decode(&encoded), Ok(TEST_IDENTITY));
    }

    #[test]
    fn reserved_bytes_are_rejected() {
        let mut encoded = TEST_IDENTITY.encode();
        encoded[31] = 1;

        assert_eq!(
            RuntimeIdentity::decode(&encoded),
            Err(IdentityDecodeError::ReservedBytesSet)
        );
    }

    #[test]
    fn unknown_architecture_is_not_silently_accepted() {
        let mut encoded = TEST_IDENTITY.encode();
        encoded[14..16].copy_from_slice(&u16::MAX.to_le_bytes());

        assert_eq!(
            RuntimeIdentity::decode(&encoded),
            Err(IdentityDecodeError::UnknownArchitecture)
        );
    }

    #[test]
    fn compatibility_keeps_architecture_protocol_features_and_limits_distinct() {
        let requirements = RuntimeRequirements {
            architecture: Architecture::X86_64,
            required_features: FeatureSet::from_bits(0b11),
            minimum_control_frame_bytes: 4096,
        };
        let compatible = RuntimeIdentity {
            features: FeatureSet::from_bits(0b111),
            ..TEST_IDENTITY
        };
        assert_eq!(requirements.check(compatible), Ok(()));

        assert!(matches!(
            requirements.check(RuntimeIdentity {
                architecture: Architecture::Aarch64,
                ..compatible
            }),
            Err(RuntimeCompatibilityError::Architecture { .. })
        ));
        assert!(matches!(
            requirements.check(RuntimeIdentity {
                protocol_major: PROTOCOL_MAJOR + 1,
                ..compatible
            }),
            Err(RuntimeCompatibilityError::ProtocolMajor { .. })
        ));
        assert!(matches!(
            requirements.check(RuntimeIdentity {
                features: FeatureSet::from_bits(0b01),
                ..compatible
            }),
            Err(RuntimeCompatibilityError::Features { .. })
        ));
        assert!(matches!(
            requirements.check(RuntimeIdentity {
                max_control_frame_bytes: 1024,
                ..compatible
            }),
            Err(RuntimeCompatibilityError::ControlFrameLimit { .. })
        ));
    }
}
