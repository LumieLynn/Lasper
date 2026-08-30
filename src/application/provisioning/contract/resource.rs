//! Evidence returned by a host adapter after applying a managed resource.

use serde::{Deserialize, Serialize};

/// Describes what a host-side managed-resource write observed.
///
/// The provisioning workflow maps this evidence to its durable
/// `ResourceDisposition`. Keeping the two types separate prevents an adapter
/// from claiming that a resource is owned merely because it was asked to
/// write it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceApplyStatus {
    Created,
    ReplacedOwned,
    Unchanged,
    ConflictUnknownOwner,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_apply_status_has_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&ResourceApplyStatus::ConflictUnknownOwner).unwrap(),
            "\"conflict_unknown_owner\""
        );
        assert_ne!(ResourceApplyStatus::Created, ResourceApplyStatus::Unchanged);
    }
}
