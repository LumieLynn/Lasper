use crate::domain::machine::accounts::LocalAccountObservation;
use serde::{Deserialize, Serialize};

/// Information observed during execution and published after cleanup succeeds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentResult {
    /// None for native provider workflows that do not open a guest rootfs.
    pub local_accounts: Option<LocalAccountObservation>,
}
