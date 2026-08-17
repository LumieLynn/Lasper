use serde::{Deserialize, Serialize};

/// Result of applying one host-side deployment resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Created,
    ReplacedOwned,
    Unchanged,
    ConflictUnknownOwner,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_status_has_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&ApplyStatus::ConflictUnknownOwner).unwrap(),
            "\"conflict_unknown_owner\""
        );
        assert_ne!(ApplyStatus::Created, ApplyStatus::Unchanged);
    }
}
