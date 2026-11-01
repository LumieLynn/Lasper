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

impl std::fmt::Display for NativeReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready { version } => write!(f, "guest manager is ready (systemd {version})"),
            Self::NoBus { reason } => write!(f, "guest system bus is unavailable: {reason}"),
            Self::NoManager => f.write_str("guest systemd manager is unavailable"),
            Self::Denied { reason } => write!(f, "guest readiness query was denied: {reason}"),
            Self::Unresponsive { reason } => write!(f, "guest readiness query timed out: {reason}"),
            Self::Unknown { reason } => write!(f, "guest readiness is unknown: {reason}"),
        }
    }
}
