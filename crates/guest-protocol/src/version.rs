use serde::{Deserialize, Serialize};

/// A concrete schema revision. Hello selects one profile from the peers' ranges;
/// every established frame carries that selected revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// Hello keeps minor zero while its payload advertises the supported range.
    /// Established frames use the version selected for their connection.
    pub const BOOTSTRAP: Self = Self {
        major: crate::ProtocolRange::CURRENT.major,
        minor: crate::BOOTSTRAP_MANIFEST.minor,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersionMismatch {
    pub expected: ProtocolVersion,
    pub actual: ProtocolVersion,
}
