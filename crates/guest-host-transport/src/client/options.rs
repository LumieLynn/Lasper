//! Connection settings and host observation budgets.

use lasper_guest_protocol::{ControlLimits, FeatureSet};
use std::time::Duration;

pub const DEFAULT_ENDPOINT: &str = "/var/opt/lasper/guest-exec.sock";

/// Host budgets for individual operations. A supplied deadline can shorten a
/// budget. These do not set the guest's execution timeout or prepared lease.
#[derive(Clone, Debug)]
pub struct ClientTimeouts {
    pub connect_hello: Duration,
    pub prepare: Duration,
    pub operation: Duration,
    pub cleanup_observation: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            connect_hello: Duration::from_secs(1),
            prepare: Duration::from_secs(5),
            operation: Duration::from_secs(5),
            cleanup_observation: Duration::from_secs(15),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub endpoint: String,
    pub control_limits: ControlLimits,
    /// Declare capabilities needed by later Start calls here, before Hello.
    /// A possible execution timeout requires EXECUTION_TIMEOUT even if its
    /// duration will only be chosen at Start.
    pub required_features: FeatureSet,
    pub timeouts: ClientTimeouts,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.into(),
            control_limits: ControlLimits::CURRENT,
            required_features: FeatureSet::from_bits(0),
            timeouts: ClientTimeouts::default(),
        }
    }
}
