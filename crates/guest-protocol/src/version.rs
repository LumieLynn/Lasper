use crate::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

/// The wire revision understood by a peer. Both numbers must match: there is
/// no cross-minor decoding or downgrade. Optional behavior uses feature bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };

    pub const fn check(self) -> Result<(), ProtocolVersionMismatch> {
        if self.major != Self::CURRENT.major || self.minor != Self::CURRENT.minor {
            Err(ProtocolVersionMismatch {
                expected: Self::CURRENT,
                actual: self,
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersionMismatch {
    pub expected: ProtocolVersion,
    pub actual: ProtocolVersion,
}
