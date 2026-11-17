use crate::{FeatureSet, MessageKind, ProtocolVersion};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    HostToGuest,
    GuestToHost,
}
/// Selects a typed message codec within a profile. The V suffix identifies that
/// message's schema revision independently of the protocol major and minor.
/// Profiles reuse an ID while its fields and semantics remain unchanged; a
/// changed schema gets a new ID and codec, preserving the old implementation.
/// These IDs are local dispatch keys and are not serialized on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaId {
    PrepareV1,
    ReadyV1,
    ControlRequestV1,
    ControlReplyV1,
    StartV1,
    StartAcceptedV1,
    ExitV1,
    ErrorV1,
    #[cfg(test)]
    TestPrepare,
    #[cfg(test)]
    TestStart,
    #[cfg(test)]
    TestExit,
}
/// Selects the identity packet codec. Revisions follow the same convention as
/// `SchemaId`, independently of control-message schemas and protocol versions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentitySchemaId {
    IdentityV1,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredBehavior {
    WorkerPidfdBinding,
    ReadyIoHandoff,
    AcceptedDoesNotConfirmExec,
    EnvironmentV1,
    StatusOnlyExit,
    CleanupOnDisconnect,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureField {
    pub path: &'static str,
    pub requires: FeatureSet,
}
#[derive(Clone, Copy, Debug)]
pub struct FeatureDefinition {
    pub feature: FeatureSet,
    pub introduced: ProtocolVersion,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageSchema {
    pub kind: MessageKind,
    pub direction: Direction,
    pub codec: SchemaId,
    pub required_fields: &'static [&'static str],
    pub optional_fields: &'static [&'static str],
    pub gated_fields: &'static [FeatureField],
    pub required_features: FeatureSet,
}
/// Acceptance limits for one profile. Build storage ceilings are computed from
/// all registered profiles and must not replace these per-profile checks.
///
/// Prepare metadata slots are fixed in `prepare/storage.rs`. Larger view arrays
/// or account names may require revising that layout and its tests. Scratch is
/// sized separately using `prepare_unescape_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileCapacities {
    pub user_name_bytes: usize,
    pub command_arguments: usize,
    pub command_argument_bytes: usize,
    pub command_total_bytes: usize,
    pub environment_entries: usize,
    pub environment_bytes: usize,
    pub default_environment_bytes: usize,
    pub min_control_frame_bytes: u32,
    pub max_control_frame_bytes: u32,
    pub max_control_descriptors: u16,
    pub max_execution_timeout_ms: u64,
    pub max_prepared_lease_ms: u32,
}
impl ProfileCapacities {
    pub const fn prepare_unescape_bytes(&self) -> usize {
        if self.command_argument_bytes > self.environment_bytes {
            self.command_argument_bytes
        } else {
            self.environment_bytes
        }
    }
    pub const fn merged_environment_bytes(&self) -> usize {
        self.default_environment_bytes + self.environment_bytes
    }
}
/// Registered schemas and capacities. Session codecs are accessed through
/// `NegotiatedProtocol`, which also enforces the connection's limits and features.
#[doc = include_str!("../../tests/profile/visibility.md")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolProfile {
    pub(super) version: ProtocolVersion,
    pub(super) capacities: ProfileCapacities,
    pub(super) messages: &'static [MessageSchema],
    pub(super) features: FeatureSet,
    pub(super) required_features: FeatureSet,
    pub(super) identity_schema: IdentitySchemaId,
    pub(super) required_behaviors: &'static [RequiredBehavior],
}
impl ProtocolProfile {
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }
    pub const fn capacities(&self) -> &ProfileCapacities {
        &self.capacities
    }
    pub const fn messages(&self) -> &'static [MessageSchema] {
        self.messages
    }
    pub const fn features(&self) -> FeatureSet {
        self.features
    }
    pub const fn required_features(&self) -> FeatureSet {
        self.required_features
    }
    pub const fn identity_schema(&self) -> IdentitySchemaId {
        self.identity_schema
    }
    pub const fn required_behaviors(&self) -> &'static [RequiredBehavior] {
        self.required_behaviors
    }
    pub fn schema(
        &self,
        kind: MessageKind,
        direction: Direction,
    ) -> Option<&'static MessageSchema> {
        self.messages
            .iter()
            .find(|m| m.kind == kind && m.direction == direction)
    }
}
const BEHAVIORS: &[RequiredBehavior] = &[
    RequiredBehavior::WorkerPidfdBinding,
    RequiredBehavior::ReadyIoHandoff,
    RequiredBehavior::AcceptedDoesNotConfirmExec,
    RequiredBehavior::EnvironmentV1,
    RequiredBehavior::StatusOnlyExit,
    RequiredBehavior::CleanupOnDisconnect,
];
macro_rules! schema {
    ($kind:ident, $direction:ident, $codec:ident) => {
        MessageSchema {
            kind: MessageKind::$kind,
            direction: Direction::$direction,
            codec: SchemaId::$codec,
            required_fields: SchemaId::$codec.fields().required,
            optional_fields: SchemaId::$codec.fields().optional,
            gated_fields: SchemaId::$codec.fields().gated,
            required_features: FeatureSet::EMPTY,
        }
    };
}
const MESSAGES: &[MessageSchema] = &[
    schema!(PrepareRequest, HostToGuest, PrepareV1),
    schema!(PrepareReady, GuestToHost, ReadyV1),
    schema!(KeepPrepared, HostToGuest, ControlRequestV1),
    schema!(KeepPrepared, GuestToHost, ControlReplyV1),
    schema!(Recheck, HostToGuest, ControlRequestV1),
    schema!(Recheck, GuestToHost, ControlReplyV1),
    schema!(Start, HostToGuest, StartV1),
    schema!(Start, GuestToHost, StartAcceptedV1),
    schema!(Cancel, HostToGuest, ControlRequestV1),
    schema!(Cancel, GuestToHost, ControlReplyV1),
    schema!(Close, HostToGuest, ControlRequestV1),
    schema!(Close, GuestToHost, ControlReplyV1),
    schema!(Exit, GuestToHost, ExitV1),
    schema!(Error, GuestToHost, ErrorV1),
];
pub const V1_0_CAPACITIES: ProfileCapacities = ProfileCapacities {
    user_name_bytes: 32,
    command_arguments: 256,
    command_argument_bytes: 4096,
    command_total_bytes: 65536,
    environment_entries: 32,
    environment_bytes: 16384,
    default_environment_bytes: 16384,
    min_control_frame_bytes: 4096,
    max_control_frame_bytes: 1048576,
    max_control_descriptors: 4,
    max_execution_timeout_ms: u64::MAX / 2,
    max_prepared_lease_ms: 300000,
};

/// Storage ceilings for this build. Connection acceptance uses its profile.
pub const BUILD_CAPACITIES: ProfileCapacities = build_capacities(protocol_profiles());

/// Maximum combined environment storage needed by any supported profile.
/// Independent field maxima can belong to different profiles.
pub const BUILD_MERGED_ENVIRONMENT_BYTES: usize = merged_environment_ceiling(protocol_profiles());

const fn merged_environment_ceiling(profiles: &[ProtocolProfile]) -> usize {
    let mut maximum = 0;
    let mut index = 0;
    while index < profiles.len() {
        let bytes = profiles[index].capacities.merged_environment_bytes();
        if bytes > maximum {
            maximum = bytes;
        }
        index += 1;
    }
    maximum
}

const fn build_capacities(profiles: &[ProtocolProfile]) -> ProfileCapacities {
    assert!(
        !profiles.is_empty(),
        "the protocol manifest must not be empty"
    );
    let mut result = profiles[0].capacities;
    let mut index = 1;
    while index < profiles.len() {
        let capacity = profiles[index].capacities;
        macro_rules! maximum {
            ($($field:ident),+ $(,)?) => {$(
                if capacity.$field > result.$field {
                    result.$field = capacity.$field;
                }
            )+};
        }
        maximum!(
            user_name_bytes,
            command_arguments,
            command_argument_bytes,
            command_total_bytes,
            environment_entries,
            environment_bytes,
            default_environment_bytes,
            min_control_frame_bytes,
            max_control_frame_bytes,
            max_control_descriptors,
            max_execution_timeout_ms,
            max_prepared_lease_ms,
        );
        index += 1;
    }
    result
}
pub const FEATURE_MANIFEST: &[FeatureDefinition] = &[
    FeatureDefinition {
        feature: FeatureSet::WORKER_PIDFD,
        introduced: ProtocolVersion { major: 1, minor: 0 },
    },
    FeatureDefinition {
        feature: FeatureSet::EXECUTION_TIMEOUT,
        introduced: ProtocolVersion { major: 1, minor: 0 },
    },
];
static PROFILES: &[ProtocolProfile] = &[ProtocolProfile {
    version: ProtocolVersion { major: 1, minor: 0 },
    capacities: V1_0_CAPACITIES,
    messages: MESSAGES,
    features: features_for(ProtocolVersion { major: 1, minor: 0 }),
    required_features: FeatureSet::WORKER_PIDFD,
    identity_schema: IdentitySchemaId::IdentityV1,
    required_behaviors: BEHAVIORS,
}];

const fn features_for(version: ProtocolVersion) -> FeatureSet {
    let mut features = FeatureSet::EMPTY;
    let mut index = 0;
    while index < FEATURE_MANIFEST.len() {
        let definition = FEATURE_MANIFEST[index];
        if definition.introduced.major == version.major
            && definition.introduced.minor <= version.minor
        {
            features = features.union(definition.feature);
        }
        index += 1;
    }
    features
}
pub const fn protocol_profiles() -> &'static [ProtocolProfile] {
    PROFILES
}
pub fn protocol_profile(version: ProtocolVersion) -> Option<&'static ProtocolProfile> {
    PROFILES.iter().find(|p| p.version == version)
}

#[cfg(test)]
#[path = "../../tests/profile/manifest.rs"]
mod tests;
