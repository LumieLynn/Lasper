//! Read-only observations, kept separate from acceptance of an execution request.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeReadiness {
    Ready { version: String },
    NoBus { reason: String },
    NoManager,
    Denied { reason: String },
    Unresponsive { reason: String },
    Unknown { reason: String },
}

impl NativeReadiness {
    pub(crate) fn unknown(error: impl std::fmt::Display) -> Self {
        Self::Unknown {
            reason: error.to_string(),
        }
    }
}
