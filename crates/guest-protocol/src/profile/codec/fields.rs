//! Field paths understood by each typed session codec. Profiles reference these
//! declarations; they cannot change a codec's shape by changing a field list.

use crate::profile::{Direction, FeatureField, ProtocolProfile, SchemaId};
use crate::{
    ControlReplyFrame, ErrorFrame, ExitFrame, FeatureSet, MessageKind, PreparationControlFrame,
    PrepareReadyFrame, PrepareRequest, PrepareRequestFrame, SessionPayloadError,
    StartAcceptedFrame, StartRequest,
};

#[derive(Clone, Copy)]
pub(crate) struct SchemaFields {
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    pub gated: &'static [FeatureField],
}

impl SchemaId {
    pub(crate) const fn fields(self) -> SchemaFields {
        let (required, optional, gated): (
            &'static [&'static str],
            &'static [&'static str],
            &'static [FeatureField],
        ) = match self {
            Self::PrepareV1 => (
                &["user", "execution", "process", "environment"],
                &["initial_size"],
                &[],
            ),
            Self::ReadyV1 => (
                &["broker_id", "preparation_id", "remaining_lease_ms"],
                &[],
                &[],
            ),
            Self::ControlRequestV1 | Self::StartAcceptedV1 => {
                (&["broker_id", "preparation_id"], &[], &[])
            }
            Self::ControlReplyV1 => (&["broker_id", "preparation_id", "status"], &[], &[]),
            Self::StartV1 => (
                &["broker_id", "preparation_id"],
                &["terminal_size", "execution_timeout_ms"],
                &[FeatureField {
                    path: "execution_timeout_ms",
                    requires: FeatureSet::EXECUTION_TIMEOUT,
                }],
            ),
            Self::ExitV1 => (
                &[
                    "broker_id",
                    "preparation_id",
                    "execution",
                    "cleanup",
                    "reason",
                ],
                &["exit", "failure"],
                &[],
            ),
            Self::ErrorV1 => (
                &["broker_id", "operation", "code"],
                &["preparation_id", "errno", "stage"],
                &[],
            ),
            #[cfg(test)]
            Self::TestPrepare | Self::TestStart | Self::TestExit => {
                return crate::profile::schema_fixture::fields(self)
            }
        };
        SchemaFields {
            required,
            optional,
            gated,
        }
    }
}

// An unknown path is a manifest/codec mismatch, not an absent optional field.
pub(super) trait FieldPresence {
    fn field_present(&self, path: &str) -> Option<bool>;
}

impl FieldPresence for PrepareRequestFrame<'_> {
    fn field_present(&self, path: &str) -> Option<bool> {
        self.request().field_present(path)
    }
}

impl FieldPresence for PrepareRequest<'_> {
    fn field_present(&self, path: &str) -> Option<bool> {
        match path {
            "user" | "execution" | "process" | "environment" => Some(true),
            "initial_size" => Some(self.initial_size().is_some()),
            _ => None,
        }
    }
}

impl FieldPresence for StartRequest {
    fn field_present(&self, path: &str) -> Option<bool> {
        match path {
            "broker_id" | "preparation_id" => Some(true),
            "terminal_size" => Some(self.terminal_size().is_some()),
            "execution_timeout_ms" => Some(self.execution_timeout_ms().is_some()),
            _ => None,
        }
    }
}

impl FieldPresence for ExitFrame {
    fn field_present(&self, path: &str) -> Option<bool> {
        match path {
            "broker_id" | "preparation_id" | "execution" | "cleanup" | "reason" => Some(true),
            "exit" => Some(self.report().exit().is_some()),
            "failure" => Some(self.report().failure().is_some()),
            _ => None,
        }
    }
}

impl FieldPresence for ErrorFrame {
    fn field_present(&self, path: &str) -> Option<bool> {
        match path {
            "broker_id" | "operation" | "code" => Some(true),
            "preparation_id" => Some(self.preparation_id().is_some()),
            "errno" => Some(self.errno().is_some()),
            "stage" => Some(self.stage().is_some()),
            _ => None,
        }
    }
}

macro_rules! required_fields {
    ($frame:ty, $($field:literal)|+) => {
        impl FieldPresence for $frame {
            fn field_present(&self, path: &str) -> Option<bool> {
                match path {
                    $($field)|+ => Some(true),
                    _ => None,
                }
            }
        }
    };
}

required_fields!(
    PrepareReadyFrame,
    "broker_id" | "preparation_id" | "remaining_lease_ms"
);
required_fields!(PreparationControlFrame, "broker_id" | "preparation_id");
required_fields!(ControlReplyFrame, "broker_id" | "preparation_id" | "status");
required_fields!(StartAcceptedFrame, "broker_id" | "preparation_id");

impl ProtocolProfile {
    pub(crate) fn required_features_for_fields(
        &self,
        kind: MessageKind,
        direction: Direction,
        present: impl Fn(&str) -> Option<bool>,
    ) -> Result<FeatureSet, SessionPayloadError> {
        let schema = self
            .schema(kind, direction)
            .ok_or(SessionPayloadError::InvalidResponse)?;
        let mut required = schema.required_features;
        for field in schema.gated_fields {
            if present(field.path).ok_or(SessionPayloadError::InvalidResponse)? {
                required = required.union(field.requires);
            }
        }
        Ok(required)
    }

    pub fn start_required_features(
        &self,
        start: &StartRequest,
    ) -> Result<FeatureSet, SessionPayloadError> {
        self.required_features_for_fields(MessageKind::Start, Direction::HostToGuest, |path| {
            start.field_present(path)
        })
    }
}
