# Guest runtime version and capability compatibility

During the Hello exchange, the endpoints select a protocol profile for their connection. The selected profile determines which messages they can exchange and how each message is interpreted.

This document defines negotiation and the profile registry in `lasper-guest-protocol`. The [README](README.md) specifies the execution messages.

## Negotiation data and public results

The host advertises its supported profiles in a `HelloRequest`, and the guest includes its own offer in the `HelloResponse`. Both endpoints use these offers to determine the version they will use.

Each binary implements one major version. Within that major version, it advertises a continuous range of minor versions for which it has working codecs. The endpoints select the highest minor version they both support. Supporting an older minor version requires retaining its codecs and behavior, even if the binary also supports newer versions.

The protocol crate defines the following types for the Hello exchange:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolRange {
    pub major: u16,
    pub min_minor: u16,
    pub max_minor: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HelloOffer {
    pub protocols: ProtocolRange,
    pub supported_features: FeatureSet,
    pub limits: ControlLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct HelloRequest {
    pub offer: HelloOffer,
    pub required_features: FeatureSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloSelection {
    pub version: ProtocolVersion,
    pub features: FeatureSet,
    pub limits: ControlLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct HelloResponse {
    pub offer: HelloOffer,
    pub selected: HelloSelection,
    pub broker_id: BrokerId,
}
```

An offer contains a protocol range, a set of supported features, and capacity limits. Its `min_minor` must not exceed its `max_minor`.

The host also specifies `required_features` in its `HelloRequest`. Because the host must implement every feature it requires, this set must be a subset of `request.offer.supported_features`. Offers sent by the guest have no `required_features` field; the bootstrap decoder rejects that field if it appears in a response or rejection.

For both `GuestClient::probe` and `GuestClient::prepare`, the host combines the client's baseline requirements with `ClientOptions.required_features`. Passing environment variables is part of the baseline and does not require an optional feature.

A `UnixControlStream` stores the result of its own Hello exchange. After validating and installing the selected protocol, the handshake code constructs a `NegotiatedHello` value as a snapshot of that exchange. Its fields are private, but the value can be copied.

```rust
#[derive(Clone, Copy, Debug)]
pub struct NegotiatedHello {
    request: HelloRequest,
    response: HelloResponse,
    negotiated: NegotiatedProtocol,
}

impl NegotiatedHello {
    pub fn request(&self) -> HelloRequest;
    pub fn response(&self) -> HelloResponse;
    pub fn limits(&self) -> EnforcedControlLimits;
    pub fn profile(&self) -> &'static ProtocolProfile;
    pub fn version(&self) -> ProtocolVersion;
    pub fn features(&self) -> FeatureSet;
    pub fn broker_id(&self) -> BrokerId;
}
```

The types in this snapshot implement `Clone`, `Copy`, and `Debug`. This includes `HelloRequest`, `HelloResponse`, and `NegotiatedProtocol`, along with the value types they contain. A `NegotiatedProtocol` holds a shared reference to a static `ProtocolProfile`, so copying the snapshot does not require the profile itself to implement `Clone`.

The `limits()`, `profile()`, `version()`, and `features()` accessors read the stored negotiation result. The `broker_id()` accessor instead reads the identity supplied in the response.

Copying a `NegotiatedHello` value does not copy ownership of the connection or its session. In particular, `WorkerReadiness` reads the negotiation record installed in its own stream; a caller cannot substitute a snapshot from another stream. `RuntimeInfo` can therefore implement `Clone` and `Debug` without acquiring any control over the connection.

The `hello` field in the `RuntimeInfo` returned by `probe()` describes that probe's exchange. For an execution session, `Session::runtime_info()` describes the separate exchange performed during preparation. In both cases, the stream's negotiation state is established by its internal handshake.

The client crate re-exports the protocol types needed to describe capabilities and compatibility errors, including `FeatureSet`, `ProtocolVersion`, `ProtocolRange`, `ProtocolProfile`, `BrokerId`, and `CompatibilityError`. Ordinary callers use `ClientOptions` to configure requirements and `RuntimeInfo` to inspect the result. The offer and selection types are also available to the raw interfaces and protocol tests.

## Minor and schema manifest

The `profile` module registers a static `ProtocolProfile` for each supported version. Each profile identifies the message schemas and features available in that version, together with its capacities and required behavior. The continuous range of registered profiles determines the version range advertised during Hello.

Profiles are constructed by the registry and can be inspected through these interfaces:

```rust
#[derive(Debug)]
pub struct ProtocolProfile { /* private */ }

pub fn protocol_profiles() -> &'static [ProtocolProfile];
pub fn protocol_profile(version: ProtocolVersion)
    -> Option<&'static ProtocolProfile>;

impl ProtocolProfile {
    pub fn version(&self) -> ProtocolVersion;
    pub fn capacities(&self) -> &'static ProfileCapacities;
    pub fn messages(&self) -> &'static [MessageSchema];
    pub fn features(&self) -> FeatureSet;
    pub fn required_features(&self) -> FeatureSet;
    pub fn identity_schema(&self) -> IdentitySchemaId;
    pub fn required_behaviors(&self) -> &'static [RequiredBehavior];
}

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

#[derive(Clone, Copy, Debug)]
pub struct NegotiatedProtocol { /* private */ }

impl NegotiatedProtocol {
    pub fn profile(&self) -> &'static ProtocolProfile;
    pub fn features(&self) -> FeatureSet;
    pub fn limits(&self) -> EnforcedControlLimits;
    pub fn selection(&self) -> HelloSelection;
}
```

A `NegotiatedProtocol` retains the selected `&'static ProtocolProfile` together with the enabled features and effective limits. Its `selection()` method returns the version, feature bits, and limits that must appear on the wire. The `NegotiatedHello::version()` accessor reads the version from this same profile.

A profile's `ProfileCapacities` is the authoritative record of its field limits and maximum frame size. The table below gives the values for profile 1.0. Validation and workspace calculations use the capacities of the profile selected for the connection.

The layout module also checks the size and alignment of the Rust types that occupy the workspace. The [Prepare metadata bound](README.md#prepare-metadata-workspace-bound) describes those checks for profile 1.0.

| Field | 1.0 |
| --- | --- |
| `user_name_bytes` | 32 |
| `command_arguments` | 256 arguments after `argv[0]`. |
| `command_argument_bytes` | 4096 |
| `command_total_bytes` | 64 * 1024, using the [argv byte rules](../guest-host-transport/README.md#execution-requests) |
| `environment_entries` | 32 |
| `environment_bytes` | 16 * 1024, including each '=' and NUL |
| `default_environment_bytes` | 16 * 1024, including each '=' and NUL |
| `min_control_frame_bytes` | 4096, including header |
| `max_control_frame_bytes` | 1024 * 1024, including header |
| `max_control_descriptors` | 4 |
| `max_execution_timeout_ms` | `u64::MAX / 2` |
| `max_prepared_lease_ms` | 300000 ms, with a default guest lease of 45000 ms. |

The [control capacity contract](README.md#control-capacities-and-hello) defines how frame size and FD count are negotiated. Each effective limit is the minimum of the two advertised values and the selected profile's ceiling. Field capacities, by contrast, are taken directly from the profile rather than negotiated independently.

For profile 1.0, `max(command_argument_bytes, environment_bytes)` requires 16 KiB of decoding scratch. Adding `default_environment_bytes` and `environment_bytes` requires 32 KiB for the merged environment. The workspace layout must provide enough storage for both.

An endpoint may advertise capacities larger than those supported by the receiving build. This allows a newer build with more storage to negotiate a shared profile with an older build. The advertised frame capacity must be at least 32 bytes, and the frame and FD limits must fit their wire types, `u32` and `u16` respectively.

The receiver does not allocate storage according to an untrusted offer. It uses only the selected limits, which are bounded by its local profile. When constructing its own offer, an endpoint derives the defaults from the build manifest and clamps any user-supplied limits to what that build supports.

Constants such as `MAX_COMMAND_*` and `MAX_PROCESS_ENVIRONMENT_*` describe storage reserved by the local build. Each is derived from the largest corresponding requirement among the supported profiles. Likewise, `MAX_PREPARE_UNESCAPE_BYTES` is the largest derived scratch requirement, while the maximum merge allocation covers the largest sum of default and additional environment bytes.

Although storage may be reserved to these build-wide ceilings, a codec must still validate a request against the selected profile. Adding a profile therefore does not enlarge the requests accepted by an existing profile. If the new profile needs more storage, its registration must be accompanied by the corresponding changes to memory budgets and layout assertions.

The layout of preparation metadata is defined separately in `src/prepare/storage.rs`; increasing `BUILD_CAPACITIES` does not automatically enlarge it. The current layout reserves two 16 KiB request slots, 8 KiB for `DecoderWorkspace`, and 8 KiB of spare space. It also checks each request against the 11296-byte estimate for profile 1.0.

If a profile increases `command_arguments`, `environment_entries`, or `user_name_bytes`, recheck the concrete type sizes. Any requirement that exceeds an existing allowance must be reflected in the slot sizes, offsets, assertions, and layout tests. Increasing only the byte allowance for strings does not increase the number of borrowed views, so it need not enlarge the metadata slots. It can still increase the scratch requirement. The runtime allocates space for scratch and metadata together, rounding the mapping size up to a multiple of 64 KiB.

Before Hello, the host checks a `ProcessSpec` for invalid syntax or values that exceed the build's storage ceiling. Once negotiation completes, it checks the request against the selected profile. If a field exceeds that profile's capacity, preparation returns `PrepareError::InvalidRequest` before sending a request or creating a worker. If the fields fit but the complete encoded frame is too large, it returns `PrepareError::Capacity` instead. The execution timeout supplied at start is also checked against the selected profile.

The manifest uses the following types to register schemas and their requirements:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    HostToGuest,
    GuestToHost,
}

#[derive(Debug)]
pub struct MessageSchema {
    pub kind: MessageKind,
    pub direction: Direction,
    pub codec: SchemaId,
    pub required_fields: &'static [&'static str],
    pub optional_fields: &'static [&'static str],
    pub gated_fields: &'static [FeatureField],
    pub required_features: FeatureSet,
}

#[derive(Debug)]
pub struct FeatureField {
    pub path: &'static str,
    pub requires: FeatureSet,
}

#[derive(Debug)]
pub struct FeatureDefinition {
    pub feature: FeatureSet,
    pub introduced: ProtocolVersion,
}

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
}

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
```

The field paths make schema registrations inspectable and allow consistency tests to compare them with the codecs. Each `SchemaId` selects a typed codec that validates the complete message structure, including its conditional fields. For example, the `execution` field in `PrepareV1` accepts `"terminal"` or `"piped"`, and only the former permits and requires `initial_size`. The [execution encoding rules](README.md#execution-kind-encoding) define that combination.

The codecs define the field lists referenced by the manifest. When several profiles use the same schema, they share its `SchemaId` and codec. A change to that schema's fields or meaning requires a new codec, while profiles that use the earlier schema retain their existing implementation.

The `V` suffix identifies a revision of an individual message schema, independently of the protocol's major and minor version numbers. For example, if protocol 1.1 changes only the preparation request, it can use `PrepareV2` while continuing to use `StartV1` and `ExitV1`.

A `SchemaId` is used only for local dispatch and is never transmitted. The receiver first selects the profile indicated by the frame header, then uses the message kind and direction to find its codec. The separate identity channel follows the same revision rule through `IdentitySchemaId`.

Profile 1.0 is registered as follows:

| Profile | Established messages | Features and introduction | Minimum frame | Identity schema | Required behavior |
| --- | --- | --- | --- | --- | --- |
| 1.0 | `PrepareRequest`, `PrepareReady`, `KeepPrepared`, `Recheck`, `Start`, `Cancel`, `Close`, `Exit`, and `Error`, with requests and responses registered by direction. | `WORKER_PIDFD` uses bit 0 and `EXECUTION_TIMEOUT` uses bit 1. Both were introduced in 1.0. | 4096 bytes, including the header. | `IdentityV1` | The worker's identity is bound to its pidfd. Preparation delivers the I/O descriptors, while start acceptance does not confirm execution. Environment names are sorted and unique, and caller values override defaults. Exit reports carry only status. Disconnect triggers cleanup. |

Profile 1.0 requires `WORKER_PIDFD` but makes `EXECUTION_TIMEOUT` optional. The `FEATURE_MANIFEST` records when each feature was introduced, from which `profile.features()` derives the available feature mask. A build advertises the optional features it actually implements and must support every behavior required by its profiles.

The [message definitions](README.md#messages-and-fields) specify the payloads and FD roles associated with this registration.

All profiles share a `BOOTSTRAP_MANIFEST` for the Hello exchange. It registers `HelloRequest` in the host-to-guest direction and `HelloResponse` and `HelloRejected` in the reverse direction. The bootstrap limit is 1024 bytes for the complete frame, leaving 992 bytes for the payload, and no bootstrap message may carry an FD.

## Profile use in codecs and state machines

Once a connection is established, callers encode and decode session messages through `NegotiatedProtocol`. The codec helpers on `ProtocolProfile` are internal because they do not, by themselves, enforce the limits and features negotiated for a particular connection.

```rust
impl NegotiatedProtocol {
    pub fn encode_prepare_frame(
        &self,
        frame: &PrepareRequestFrame<'_>,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError>;

    pub fn decode_exit(
        &self,
        request: &PrepareRequestFrame<'_>,
        preparation: PreparationId,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<ExitFrame, SessionFrameError>;
}
```

The same dispatch rules apply to the other session messages. `NegotiatedProtocol` looks up the message kind and direction in the selected profile, checks the negotiated requirements, and calls the codec identified by its `SchemaId`. That codec rejects fields or enum values that the schema does not define. If a request needs a field that the selected profile cannot express, the host rejects it before submission.

Callers that encode a preparation or start payload separately from its header must still use `NegotiatedProtocol` and include the header size when checking frame capacity. Validating a header checks requirements for the message as a whole; only decoding the complete message can check whether a gated field was present.

The identity channel has its own codecs, selected through `identity_schema`. Because it is separate from the control stream, its messages are not bounded by the control frame limit.

The guardian uses the negotiation result to enforce the selected profile's behavior. Although the minimum frame capacity is 4096 bytes, an individual preparation request may need more space. The host checks its complete encoding before sending it and reports `PrepareError::Capacity` if it does not fit, as described in the [API error contract](../guest-host-transport/README.md#errors-and-cancellation).

## Responsibility of required_features

The selected profile defines the capabilities that both endpoints must provide through `profile.required_features()`. The host can require additional capabilities for a particular connection through `HelloRequest.required_features`. Each endpoint's `supported_features` set advertises what it has actually implemented.

For profile 1.0, both endpoints must support `WORKER_PIDFD`. The host also needs `EXECUTION_TIMEOUT` if it intends to impose an execution budget. The guest has no additional requirement to place on the host beyond those of the profile, so its offer contains no independent set of required features.

A requirement that applies to every connection using a minor version belongs in that profile. A requirement arising from a particular host operation belongs in the host's request. If the host requires a feature it does not itself support, its offer is invalid. If it supports that feature but the guest does not, negotiation fails with `MissingFeatures`.

## Selection rules

The guest calculates the selection with the pure function below. The host later calls the same function to verify that the response contains the expected result.

```rust
pub fn negotiate_hello(
    request: HelloRequest,
    guest: HelloOffer,
) -> Result<NegotiatedProtocol, CompatibilityError>;
```

The `negotiate_hello` function reads the host's offer and requirements from the request. The guest passes its local offer as the second argument. To verify the result, the host calls the function with its original request and the guest's offer from the response, then compares the result with `response.selected`. Only after this check does the host install the result in the stream and enter the `Established` state.

1. Validate both offers and their version ranges. The host's `required_features` must be a subset of its `supported_features`; otherwise, return `InvalidOffer`. Then check that the major versions match.
2. Let `host_range = request.offer.protocols` and `guest_range = guest.protocols`. Compute `low = max(host_range.min_minor, guest_range.min_minor)` and `high = min(host_range.max_minor, guest_range.max_minor)`. If `low > high`, there is no common minor version. Otherwise, select the local profile for `high`. If that version has no registered profile or codec, return `InvalidOffer` rather than falling back to a lower version.
3. Intersect the endpoints' `supported_features` sets with `profile.features()` to obtain `enabled_features`.
4. Form `required_features = request.required_features ∪ profile.required_features()`. If this is not a subset of `enabled_features`, return `MissingFeatures`, using the union for `required` and the enabled set for `available`.
5. For both frame size and FD count, take the minimum of the endpoints' offers and the profile ceiling. The resulting frame capacity must be at least `profile.capacities().min_control_frame_bytes`. Return a `NegotiatedProtocol` bound to that profile and those limits. These limits apply after negotiation; the Hello exchange itself remains subject to `BOOTSTRAP_MANIFEST`.

The guest includes the value returned by `selection()` in its `HelloResponse`. After verifying that selection, the host transport stores it in the stream, where `WorkerReadiness` can read it.

Before submitting a preparation request, the host checks that the connection can carry all FDs needed by its response. For example, a negotiated limit of three permits terminal execution, but piped execution returns `PrepareError::Capacity(ReadyDescriptors { required: 4, available: 3 })`.

Range intersection examples:

| Host range | Guest range | Result |
| --- | --- | --- |
| 1.[0,3] | 1.[1,2] | Select 1.2 |
| 1.[2,3] | 1.[0,1] | Reject: no common minor |
| 1.[0,3] | 2.[0,3] | Reject: different majors |
| Both support 1.2, but a host/profile-required feature is missing | Same | Reject before Prepare |

## Minor versions, features, and fields

A minor version determines the required fields and behavior of a connection, including the state machine it follows. Features make specific capabilities optional within that version. Each feature has a fixed bit and an introduction version, and it can be enabled only when both endpoints and the selected profile support it.

The client uses the following capability requirements:

| Capability | Requirement | API effect |
| --- | --- | --- |
| `WORKER_PIDFD` | The client always requires this feature. | It binds identity observations to a pinned worker so that the host can verify its target membership. |
| `EXECUTION_TIMEOUT` | The client requires this feature when imposing an execution budget. | It lets the guest terminate execution when that budget expires. |

Profile 1.0 always supports passing environment variables and delivering I/O descriptors during preparation. Its exit reports carry status rather than output. None of these behaviors depends on an optional feature.

The caller supplies `execution_timeout` when starting a session. If `EXECUTION_TIMEOUT` was not enabled during negotiation, the client returns `NotSubmitted(UnsupportedFeatures)` and leaves the session prepared. A caller that wants to establish support before preparing a worker can add this feature to `ClientOptions.required_features`.

An encoder may include a gated field only if its feature is enabled; otherwise, it must omit the field. The decoder treats an unnegotiated field, message, or enum variant as a protocol error. Both sides apply these rules using the schema selected for their connection.

Existing profiles retain their original fields and behavior. An optional capability is registered as a feature, whereas a change to required behavior needs a new minor version. Removing an older profile raises the minimum supported minor version. A change that cannot preserve compatibility within the major version requires a new major version.

## Hello wire format and connection state

The Hello exchange uses the [control frame format](README.md#control-frame-format), with the fields and 1024-byte limit defined by `BOOTSTRAP_MANIFEST`. The header's major version matches the sender's offer, but its minor version is always the bootstrap value zero. The payload, rather than the header, advertises the supported range.

This distinction allows the endpoints to negotiate even if neither supports execution profile 1.0. The bootstrap schema stays fixed as newer session profiles are introduced.

A `HelloResponse` also uses a bootstrap header. Once the handshake completes, the stream encodes and validates every control header against the selected major and minor version.

The separate identity channel uses the schema registered in the selected profile. This live exchange is distinct from the build metadata emitted by `--identify`, which is described in the next section.

If negotiation fails, the guest sends a `HelloRejected` response, whose message kind is 12. Its payload uses the following types:

```rust
#[derive(Clone, Copy, Debug)]
pub struct HelloRejected {
    pub offer: HelloOffer,
    pub reason: CompatibilityError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatibilityError {
    MajorMismatch {
        local: u16,
        peer: u16,
    },
    NoCommonMinor {
        local: ProtocolRange,
        peer: ProtocolRange,
    },
    MissingFeatures {
        required: FeatureSet,
        available: FeatureSet,
    },
    InsufficientFrameCapacity {
        required: u32,
        available: u32,
    },
    InvalidOffer,
    InvalidSelection,
}
```

After sending a `HelloRejected` response with the bootstrap encoding, the guest closes the connection. The host validates the offer and rejection reason, interpreting `local` and `peer` from the host's perspective. If the frame's magic or framing format cannot be recognized, the receiver closes the connection without attempting to interpret a rejection.

Only the guest may send `HelloRejected`. If the host finds that the guest selected an invalid result, it reports `Incompatible(InvalidSelection)` locally and closes the connection; it does not send a rejection back. Accordingly, `InvalidSelection` is not a valid value for `HelloRejected.reason`. A rejection sent in the wrong direction, or one containing an invalid reason, is a protocol error.

When framing is recognizable, the bootstrap exchange can report a major-version mismatch. The [wire state machine](README.md#wire-state-machine) defines when each endpoint becomes established and how it handles subsequent messages or disconnection.

## Static RuntimeIdentity and live negotiation

The runtime's `--identify` option emits a fixed 32-byte record for validating a build artifact. The version in this record identifies the highest profile compiled into the artifact. It does not advertise the range available on a live connection; that range is established through Hello.

The record is represented by `RuntimeIdentity`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeIdentity {
    pub architecture: Architecture,
    pub build_protocol_version: ProtocolVersion,
    pub compiled_features: FeatureSet,
    pub compiled_max_control_frame_bytes: u32,
}

impl RuntimeIdentity {
    pub const fn current(architecture: Architecture) -> Self;
    pub const fn encode(self) -> [u8; IDENTITY_FRAME_BYTES];
    pub fn decode(frame: &[u8]) -> Result<Self, IdentityDecodeError>;
}
```

The `build_protocol_version` field comes from the highest profile in the build manifest. The other fields describe compiled capabilities and the maximum frame size that the build can store. These values do not prove that the deployed kernel supports those capabilities, so the host still performs readiness checks and negotiates through Hello.

The record uses the following byte layout:

| Offset | Contents |
| --- | --- |
| 0..8 | identity magic |
| 8..10 | Identity format, encoded as a little-endian `u16`. |
| 10..12 | `build_protocol_version.major`, encoded as a little-endian `u16`. |
| 12..14 | `build_protocol_version.minor`, encoded as a little-endian `u16`. |
| 14..16 | `architecture`, encoded as a little-endian `u16`. |
| 16..24 | `compiled_features`, encoded as a little-endian `u64`. |
| 24..28 | `compiled_max_control_frame_bytes`, encoded as a little-endian `u32`. |
| 28..32 | Reserved, must be zero |

A call to `GuestClient::probe` checks a live broker. A later call to `prepare()` negotiates again on its own connection rather than trusting that earlier result. Either call can fail with a connection or compatibility error.

The artifact verifier requires the emitted metadata to match the expected build metadata exactly.

## Compatibility errors and validation

A failed negotiation closes the connection. Errors encountered while connecting or reading frames retain their own categories rather than being converted into compatibility errors. After successful negotiation, the profile remains fixed for the lifetime of the connection.

The `CompatibilityError` variants distinguish the following failures:

| `Error` | Condition |
| --- | --- |
| `MajorMismatch` | The endpoints advertise different major versions. |
| `NoCommonMinor` | Their valid version ranges have no minor version in common. |
| `MissingFeatures` | The enabled features do not include every feature required by the host and the selected profile. |
| `InsufficientFrameCapacity` | The effective frame limit is below `profile.capacities().min_control_frame_bytes`. |
| `InvalidOffer` | A range is reversed, the host requires a feature it does not advertise as supported, or a locally advertised version lacks a registered profile or codec. |
| `InvalidSelection` | The host finds that the received selection differs from the result of `negotiate_hello()`. This error is local and cannot be sent to the guest in `HelloRejected`. |

## Tests

Compatibility tests live under `tests/profile/` and run with `cargo test -p lasper-guest-protocol --locked`. Their fixture profiles exercise negotiation between versions, including cases where both peers must use a lower profile despite one supporting a higher version. The checks ensure that the selected codec and limits belong to that lower profile.

When registering a profile, test its field rules and feature gates in exchanges with every supported peer profile. Those tests must also establish that adding the new profile leaves the acceptance and encoding rules of existing profiles unchanged.

The current baseline is protocol 1.0, and the host and guest are built and distributed together. Any later supported version must have its own registration in the manifest.
