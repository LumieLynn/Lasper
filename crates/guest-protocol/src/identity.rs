//! Fixed build-artifact metadata returned by `--identify`. This format is
//! independent of Hello and does not establish live runtime compatibility.

use crate::{FeatureSet, ProtocolVersion};

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

/// Build artifact metadata. Live compatibility is established only by Hello.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeIdentity {
    pub architecture: Architecture,
    /// Highest profile registered in this artifact, not a connection selection.
    pub build_protocol_version: ProtocolVersion,
    pub compiled_features: FeatureSet,
    pub compiled_max_control_frame_bytes: u32,
}

impl RuntimeIdentity {
    pub const fn current(architecture: Architecture) -> Self {
        Self::from_profiles(architecture, crate::protocol_profiles())
    }

    pub(crate) const fn from_profiles(
        architecture: Architecture,
        profiles: &[crate::ProtocolProfile],
    ) -> Self {
        assert!(
            !profiles.is_empty(),
            "the protocol manifest must not be empty"
        );
        let mut build_protocol_version = profiles[0].version();
        let mut compiled_features = FeatureSet::EMPTY;
        let mut compiled_max_control_frame_bytes = 0;
        let mut index = 0;
        while index < profiles.len() {
            let profile = &profiles[index];
            let version = profile.version();
            if version.major > build_protocol_version.major
                || (version.major == build_protocol_version.major
                    && version.minor > build_protocol_version.minor)
            {
                build_protocol_version = version;
            }
            compiled_features = compiled_features.union(profile.features());
            let frame_bytes = profile.capacities().max_control_frame_bytes;
            if frame_bytes > compiled_max_control_frame_bytes {
                compiled_max_control_frame_bytes = frame_bytes;
            }
            index += 1;
        }
        Self {
            architecture,
            build_protocol_version,
            compiled_features,
            compiled_max_control_frame_bytes,
        }
    }

    pub const fn encode(self) -> [u8; IDENTITY_FRAME_BYTES] {
        let mut frame = [0; IDENTITY_FRAME_BYTES];
        let format = IDENTITY_FORMAT.to_le_bytes();
        let major = self.build_protocol_version.major.to_le_bytes();
        let minor = self.build_protocol_version.minor.to_le_bytes();
        let architecture = (self.architecture as u16).to_le_bytes();
        let features = self.compiled_features.bits().to_le_bytes();
        let max_frame = self.compiled_max_control_frame_bytes.to_le_bytes();

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
            build_protocol_version: ProtocolVersion {
                major: read_u16(frame, 10),
                minor: read_u16(frame, 12),
            },
            architecture: Architecture::from_wire(read_u16(frame, 14))?,
            compiled_features: FeatureSet::from_bits(read_u64(frame, 16)),
            compiled_max_control_frame_bytes: read_u32(frame, 24),
        })
    }
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
#[path = "../tests/identity/artifact.rs"]
mod tests;
