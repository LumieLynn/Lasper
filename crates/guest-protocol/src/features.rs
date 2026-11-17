//! Capability bits shared by artifact metadata, Hello and profile field gates.

use serde::{Deserialize, Serialize};

/// Preserves unknown bits in peer offers and artifact metadata. Negotiation
/// intersects offers with the selected profile before enabling any capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FeatureSet(u64);

impl FeatureSet {
    pub const EMPTY: Self = Self(0);

    /// Ready transfers a worker pidfd acquired by the guardian before the
    /// child's PID can be reused. The host must support checking this descriptor.
    /// The 1.0 profile requires both peers to support this contract.
    pub const WORKER_PIDFD: Self = Self(1 << 0);
    /// Allows Start to carry an explicit execution budget enforced by the guardian.
    pub const EXECUTION_TIMEOUT: Self = Self(1 << 1);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

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
