use heapless::String;
use serde::{Deserialize, Serialize};

use crate::messages::{decode_json_payload, encode_json_payload};
use crate::prepare::PrepareRequest;
use crate::{PayloadCodecError, PreparationId, MAX_GUEST_USER_NAME_BYTES};

pub const IDENTITY_CHALLENGE_BYTES: usize = 32;
pub const MAX_IDENTITY_MESSAGE_BYTES: usize = 64 * 1024;

/// The worker echoes both fields so the host can associate its reply with
/// the current preparation and challenge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct IdentityChallenge {
    preparation_id: PreparationId,
    nonce: [u8; IDENTITY_CHALLENGE_BYTES],
}

impl IdentityChallenge {
    pub(crate) const fn new(
        preparation_id: PreparationId,
        nonce: [u8; IDENTITY_CHALLENGE_BYTES],
    ) -> Self {
        Self {
            preparation_id,
            nonce,
        }
    }

    pub const fn preparation_id(self) -> PreparationId {
        self.preparation_id
    }

    pub const fn nonce(&self) -> &[u8; IDENTITY_CHALLENGE_BYTES] {
        &self.nonce
    }

    pub fn encode_packet(self, output: &mut [u8]) -> Result<usize, IdentityChannelError> {
        encode_packet(&self, output)
    }

    pub fn decode_packet(bytes: &[u8]) -> Result<Self, IdentityChannelError> {
        let wire: WireIdentityChallenge = decode_packet(bytes)?;
        Ok(Self::new(wire.preparation_id, wire.nonce))
    }
}

/// The worker identifies the socket with a symlink-following `stat`, then
/// checks `W_OK` under the target account's effective UID, GID and supplementary
/// groups, matching the native probe's `test -w`. This reports filesystem write
/// access; X server authorization is checked separately.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SocketPathAccess {
    /// The path is a socket and the write-access check succeeded.
    Accessible,
    /// The path is a socket but the write-access check failed.
    Denied,
    /// `stat` resolved the path to a file of another type.
    NotSocket,
    /// `stat` failed; the path may be absent or a parent may be unsearchable.
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SocketPathIdentity {
    device: u64,
    inode: u64,
}

impl SocketPathIdentity {
    pub const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Denied sockets retain their identity so the host can distinguish a stale
/// bind from a permissions problem.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SocketPathObservation {
    access: SocketPathAccess,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<SocketPathIdentity>,
}

impl SocketPathObservation {
    pub fn new(
        access: SocketPathAccess,
        identity: Option<SocketPathIdentity>,
    ) -> Result<Self, PayloadCodecError> {
        let observation = Self { access, identity };
        observation.validate()?;
        Ok(observation)
    }

    pub const fn access(self) -> SocketPathAccess {
        self.access
    }

    pub const fn identity(self) -> Option<SocketPathIdentity> {
        self.identity
    }

    fn validate(self) -> Result<(), PayloadCodecError> {
        match (self.access, self.identity) {
            (SocketPathAccess::Accessible | SocketPathAccess::Denied, Some(_))
            | (SocketPathAccess::NotSocket | SocketPathAccess::Unavailable, None) => Ok(()),
            _ => Err(PayloadCodecError::InvalidRequest),
        }
    }
}

/// Paths are fixed by Prepare and are not repeated in the reply. The worker
/// observes `WaylandProjectionRequest::target()` for `wayland`; X11's field-to-path
/// mapping is defined by [`X11PathObservations`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProjectionObservations {
    #[serde(skip_serializing_if = "Option::is_none")]
    wayland: Option<SocketPathObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    x11: Option<X11PathObservations>,
}

impl ProjectionObservations {
    pub const fn new(
        wayland: Option<SocketPathObservation>,
        x11: Option<X11PathObservations>,
    ) -> Self {
        Self { wayland, x11 }
    }

    pub const fn wayland(self) -> Option<SocketPathObservation> {
        self.wayland
    }

    pub const fn x11(self) -> Option<X11PathObservations> {
        self.x11
    }

    pub const fn is_empty(&self) -> bool {
        self.wayland.is_none() && self.x11.is_none()
    }

    fn validate_for(self, request: &PrepareRequest<'_>) -> Result<(), IdentityChannelError> {
        let projections = request.projections();
        if self.wayland.is_some() != projections.wayland().is_some()
            || self.x11.is_some() != projections.x11().is_some()
        {
            return Err(IdentityChannelError::ProjectionMismatch);
        }
        Ok(())
    }
}

/// The worker observes `X11ProjectionRequest::mount_target()` for `mount` and
/// `X11ProjectionRequest::client_path()` for `client`, under the target account.
/// Keeping these results separate lets the host detect a correct bind whose
/// guest client path points to a different socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct X11PathObservations {
    mount: SocketPathObservation,
    client: SocketPathObservation,
}

impl X11PathObservations {
    pub const fn new(mount: SocketPathObservation, client: SocketPathObservation) -> Self {
        Self { mount, client }
    }

    pub const fn mount(self) -> SocketPathObservation {
        self.mount
    }

    pub const fn client(self) -> SocketPathObservation {
        self.client
    }
}

/// UID and GID here are guest-reported values. The kernel supplies sender
/// credentials separately through SCM_CREDENTIALS.
///
/// Decoding needs the pending request and challenge. Only private wire types
/// implement `Deserialize`, so parsing cannot skip those context checks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuestIdentityReply {
    preparation_id: PreparationId,
    challenge: [u8; IDENTITY_CHALLENGE_BYTES],
    guest_user: String<MAX_GUEST_USER_NAME_BYTES>,
    guest_uid: u32,
    guest_gid: u32,
    #[serde(skip_serializing_if = "ProjectionObservations::is_empty")]
    projections: ProjectionObservations,
}

impl GuestIdentityReply {
    pub fn new(
        challenge: IdentityChallenge,
        request: &PrepareRequest<'_>,
        guest_user: &str,
        guest_uid: u32,
        guest_gid: u32,
        projections: ProjectionObservations,
    ) -> Result<Self, IdentityChannelError> {
        if guest_user != request.user() {
            return Err(IdentityChannelError::UserMismatch);
        }
        projections.validate_for(request)?;
        let mut bounded_user = String::new();
        bounded_user
            .push_str(guest_user)
            .map_err(|_| IdentityChannelError::Payload(PayloadCodecError::InvalidRequest))?;
        Ok(Self {
            preparation_id: challenge.preparation_id(),
            challenge: *challenge.nonce(),
            guest_user: bounded_user,
            guest_uid,
            guest_gid,
            projections,
        })
    }

    pub const fn preparation_id(&self) -> PreparationId {
        self.preparation_id
    }

    pub const fn challenge(&self) -> &[u8; IDENTITY_CHALLENGE_BYTES] {
        &self.challenge
    }

    pub fn guest_user(&self) -> &str {
        self.guest_user.as_str()
    }

    pub const fn guest_uid(&self) -> u32 {
        self.guest_uid
    }

    pub const fn guest_gid(&self) -> u32 {
        self.guest_gid
    }

    pub const fn projections(&self) -> ProjectionObservations {
        self.projections
    }

    pub fn encode_packet(&self, output: &mut [u8]) -> Result<usize, IdentityChannelError> {
        encode_packet(self, output)
    }

    /// Before using the reply, the host must verify the sender's SCM_CREDENTIALS,
    /// machine membership and ID mappings, then match the observed sockets to
    /// the expected host projections.
    pub fn decode_for(
        bytes: &[u8],
        challenge: IdentityChallenge,
        request: &PrepareRequest<'_>,
    ) -> Result<Self, IdentityChannelError> {
        let wire: WireGuestIdentityReply = decode_packet(bytes)?;
        wire.into_reply(challenge, request)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIdentityChallenge {
    preparation_id: PreparationId,
    nonce: [u8; IDENTITY_CHALLENGE_BYTES],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGuestIdentityReply {
    preparation_id: PreparationId,
    challenge: [u8; IDENTITY_CHALLENGE_BYTES],
    guest_user: String<MAX_GUEST_USER_NAME_BYTES>,
    guest_uid: u32,
    guest_gid: u32,
    #[serde(default)]
    projections: WireProjectionObservations,
}

impl WireGuestIdentityReply {
    fn into_reply(
        self,
        challenge: IdentityChallenge,
        request: &PrepareRequest<'_>,
    ) -> Result<GuestIdentityReply, IdentityChannelError> {
        if self.preparation_id != challenge.preparation_id() || self.challenge != *challenge.nonce()
        {
            return Err(IdentityChannelError::ChallengeMismatch);
        }
        GuestIdentityReply::new(
            challenge,
            request,
            self.guest_user.as_str(),
            self.guest_uid,
            self.guest_gid,
            self.projections.into_observations()?,
        )
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjectionObservations {
    wayland: Option<WireSocketPathObservation>,
    x11: Option<WireX11PathObservations>,
}

impl WireProjectionObservations {
    fn into_observations(self) -> Result<ProjectionObservations, IdentityChannelError> {
        Ok(ProjectionObservations::new(
            self.wayland
                .map(WireSocketPathObservation::into_observation)
                .transpose()?,
            self.x11
                .map(WireX11PathObservations::into_observations)
                .transpose()?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireX11PathObservations {
    mount: WireSocketPathObservation,
    client: WireSocketPathObservation,
}

impl WireX11PathObservations {
    fn into_observations(self) -> Result<X11PathObservations, IdentityChannelError> {
        Ok(X11PathObservations::new(
            self.mount.into_observation()?,
            self.client.into_observation()?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSocketPathObservation {
    access: SocketPathAccess,
    identity: Option<SocketPathIdentity>,
}

impl WireSocketPathObservation {
    fn into_observation(self) -> Result<SocketPathObservation, IdentityChannelError> {
        SocketPathObservation::new(self.access, self.identity)
            .map_err(IdentityChannelError::Payload)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityChannelError {
    Payload(PayloadCodecError),
    ChallengeMismatch,
    UserMismatch,
    ProjectionMismatch,
}

fn encode_packet<T: Serialize + ?Sized>(
    value: &T,
    output: &mut [u8],
) -> Result<usize, IdentityChannelError> {
    let capacity = output.len().min(MAX_IDENTITY_MESSAGE_BYTES);
    let output = &mut output[..capacity];
    encode_json_payload(value, output).map_err(|error| {
        if capacity == MAX_IDENTITY_MESSAGE_BYTES && error == PayloadCodecError::OutputTooSmall {
            IdentityChannelError::Payload(PayloadCodecError::PayloadTooLarge)
        } else {
            IdentityChannelError::Payload(error)
        }
    })
}

fn decode_packet<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, IdentityChannelError> {
    if bytes.len() > MAX_IDENTITY_MESSAGE_BYTES {
        return Err(IdentityChannelError::Payload(
            PayloadCodecError::PayloadTooLarge,
        ));
    }
    decode_json_payload(bytes).map_err(IdentityChannelError::Payload)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareRequest,
        ProjectionRequests, TerminalEnvironment, TerminalSize, WaylandProjectionRequest,
        X11ProjectionRequest,
    };
    use serde::ser::SerializeSeq;

    fn interactive_request() -> PrepareRequest<'static> {
        PrepareRequest::new(
            "alice",
            ExecutionKind::Interactive,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/terminal"]).unwrap()),
            Some(TerminalEnvironment::new("xterm", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::new(
                Some(WaylandProjectionRequest::new("/run/user/1000/wayland-1").unwrap()),
                Some(X11ProjectionRequest::new(0, "/mnt/host-x11", "/tmp/.X11-unix/X0").unwrap()),
            ),
        )
        .unwrap()
    }

    fn challenge() -> IdentityChallenge {
        IdentityChallenge::new(PreparationId::new([0x31; 16]), [0x72; 32])
    }

    fn accessible() -> SocketPathObservation {
        SocketPathObservation::new(
            SocketPathAccess::Accessible,
            Some(SocketPathIdentity::new(42, 91)),
        )
        .unwrap()
    }

    fn reply(request: &PrepareRequest<'_>) -> GuestIdentityReply {
        GuestIdentityReply::new(
            challenge(),
            request,
            "alice",
            1000,
            1000,
            ProjectionObservations::new(
                Some(accessible()),
                Some(X11PathObservations::new(
                    accessible(),
                    SocketPathObservation::new(
                        SocketPathAccess::Denied,
                        Some(SocketPathIdentity::new(42, 92)),
                    )
                    .unwrap(),
                )),
            ),
        )
        .unwrap()
    }

    fn wire_reply(guest_user: &str, projections_json: &str) -> std::vec::Vec<u8> {
        let mut id = [0; 128];
        let id_bytes = encode_json_payload(&challenge().preparation_id(), &mut id).unwrap();
        let mut nonce = [0; 256];
        let nonce_bytes = encode_json_payload(challenge().nonce(), &mut nonce).unwrap();
        std::format!(
            r#"{{"preparation_id":{},"challenge":{},"guest_user":"{guest_user}","guest_uid":1000,"guest_gid":1000,"projections":{projections_json}}}"#,
            std::str::from_utf8(&id[..id_bytes]).unwrap(),
            std::str::from_utf8(&nonce[..nonce_bytes]).unwrap(),
        )
        .into_bytes()
    }

    #[test]
    fn identity_reply_round_trips_challenge_credentials_and_requested_paths() {
        let request = interactive_request();
        let expected = reply(&request);
        let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];

        let mut challenge_bytes = [0; 256];
        let challenge_length = challenge().encode_packet(&mut challenge_bytes).unwrap();
        assert_eq!(
            IdentityChallenge::decode_packet(&challenge_bytes[..challenge_length]).unwrap(),
            challenge()
        );

        let length = expected.encode_packet(&mut bytes).unwrap();
        let decoded =
            GuestIdentityReply::decode_for(&bytes[..length], challenge(), &request).unwrap();

        assert_eq!(decoded, expected);
        assert_eq!(decoded.guest_user(), "alice");
        assert_eq!((decoded.guest_uid(), decoded.guest_gid()), (1000, 1000));
        assert_eq!(
            decoded
                .projections()
                .wayland()
                .unwrap()
                .identity()
                .unwrap()
                .inode(),
            91
        );
        assert_eq!(
            decoded.projections().x11().unwrap().client().access(),
            SocketPathAccess::Denied
        );
        let x11 = decoded.projections().x11().unwrap();
        assert_eq!(
            x11.mount().identity(),
            Some(SocketPathIdentity::new(42, 91))
        );
        assert_eq!(
            x11.client().identity(),
            Some(SocketPathIdentity::new(42, 92))
        );
    }

    #[test]
    fn identity_reply_rejects_wrong_challenge_user_and_projection_set() {
        let request = interactive_request();
        let valid = reply(&request);
        let mut bytes = [0; 1024];
        let length = valid.encode_packet(&mut bytes).unwrap();

        let wrong_challenge =
            IdentityChallenge::new(PreparationId::new([0x32; 16]), *challenge().nonce());
        assert_eq!(
            GuestIdentityReply::decode_for(&bytes[..length], wrong_challenge, &request),
            Err(IdentityChannelError::ChallengeMismatch)
        );

        let wrong_nonce = IdentityChallenge::new(
            challenge().preparation_id(),
            [0x73; IDENTITY_CHALLENGE_BYTES],
        );
        assert_eq!(
            GuestIdentityReply::decode_for(&bytes[..length], wrong_nonce, &request),
            Err(IdentityChannelError::ChallengeMismatch)
        );

        let wrong_user = GuestIdentityReply::new(
            challenge(),
            &request,
            "bob",
            1000,
            1000,
            ProjectionObservations::new(
                Some(accessible()),
                Some(X11PathObservations::new(accessible(), accessible())),
            ),
        )
        .unwrap_err();
        assert_eq!(wrong_user, IdentityChannelError::UserMismatch);

        let missing_projection = GuestIdentityReply::new(
            challenge(),
            &request,
            "alice",
            1000,
            1000,
            ProjectionObservations::new(Some(accessible()), None),
        )
        .unwrap_err();
        assert_eq!(missing_projection, IdentityChannelError::ProjectionMismatch);

        let all_paths = r#"{"wayland":{"access":"unavailable"},"x11":{"mount":{"access":"unavailable"},"client":{"access":"unavailable"}}}"#;
        assert_eq!(
            GuestIdentityReply::decode_for(&wire_reply("bob", all_paths), challenge(), &request),
            Err(IdentityChannelError::UserMismatch)
        );
        assert_eq!(
            GuestIdentityReply::decode_for(&wire_reply("alice", "{}"), challenge(), &request),
            Err(IdentityChannelError::ProjectionMismatch)
        );
    }

    #[test]
    fn bounded_identity_reply_rejects_unrequested_projection_on_construction_and_decode() {
        let request = PrepareRequest::new(
            "alice",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&["/sbin/poweroff"]).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap();
        let extra_projection = GuestIdentityReply::new(
            challenge(),
            &request,
            "alice",
            0,
            0,
            ProjectionObservations::new(Some(accessible()), None),
        )
        .unwrap_err();
        assert_eq!(extra_projection, IdentityChannelError::ProjectionMismatch);

        let extra_projection_packet =
            wire_reply("alice", r#"{"wayland":{"access":"unavailable"}}"#);
        assert_eq!(
            GuestIdentityReply::decode_for(&extra_projection_packet, challenge(), &request),
            Err(IdentityChannelError::ProjectionMismatch)
        );

        let valid = GuestIdentityReply::new(
            challenge(),
            &request,
            "alice",
            1000,
            1000,
            ProjectionObservations::default(),
        )
        .unwrap();
        let mut bytes = [0; 1024];
        let length = valid.encode_packet(&mut bytes).unwrap();
        assert_eq!(
            GuestIdentityReply::decode_for(&bytes[..length], challenge(), &request),
            Ok(valid)
        );
    }

    #[test]
    fn identity_decode_rejects_inconsistent_socket_observations() {
        let observation_request = PrepareRequest::new(
            "alice",
            ExecutionKind::Interactive,
            GuestProcess::LoginShell,
            Some(TerminalEnvironment::new("xterm", None, None).unwrap()),
            Some(TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::new(
                Some(WaylandProjectionRequest::new("/run/user/1000/wayland-1").unwrap()),
                None,
            ),
        )
        .unwrap();

        for observation in [
            r#"{"access":"accessible"}"#,
            r#"{"access":"denied"}"#,
            r#"{"access":"accessible","identity":null}"#,
            r#"{"access":"not_socket","identity":{"device":42,"inode":91}}"#,
            r#"{"access":"unavailable","identity":{"device":42,"inode":91}}"#,
        ] {
            let projections = std::format!(r#"{{"wayland":{observation}}}"#);
            let bytes = wire_reply("alice", &projections);
            assert_eq!(
                GuestIdentityReply::decode_for(&bytes, challenge(), &observation_request),
                Err(IdentityChannelError::Payload(
                    PayloadCodecError::InvalidRequest
                )),
                "observation: {observation}"
            );
        }
    }

    #[test]
    fn identity_decode_rejects_incomplete_x11_pairs_and_malformed_socket_identities() {
        let request = interactive_request();
        for projections in [
            r#"{"wayland":{"access":"unavailable"},"x11":{"mount":{"access":"unavailable"}}}"#,
            r#"{"wayland":{"access":"unavailable"},"x11":{"client":{"access":"unavailable"}}}"#,
            r#"{"wayland":{"access":"accessible","identity":{"device":42}},"x11":{"mount":{"access":"unavailable"},"client":{"access":"unavailable"}}}"#,
            r#"{"wayland":{"access":"unavailable"},"x11":{"mount":{"access":"unavailable"},"client":{"access":"unavailable"},"other":{"access":"unavailable"}}}"#,
        ] {
            assert_eq!(
                GuestIdentityReply::decode_for(
                    &wire_reply("alice", projections),
                    challenge(),
                    &request
                ),
                Err(IdentityChannelError::Payload(
                    PayloadCodecError::InvalidJson
                )),
                "projections: {projections}"
            );
        }
    }

    #[test]
    fn socket_observation_requires_identity_only_for_observed_socket_paths() {
        for access in [SocketPathAccess::Accessible, SocketPathAccess::Denied] {
            assert!(SocketPathObservation::new(access, None).is_err());
        }
        for access in [SocketPathAccess::NotSocket, SocketPathAccess::Unavailable] {
            assert!(
                SocketPathObservation::new(access, Some(SocketPathIdentity::new(1, 2))).is_err()
            );
        }
        assert!(SocketPathObservation::new(SocketPathAccess::Unavailable, None).is_ok());
    }

    #[test]
    fn identity_packet_decoder_enforces_the_seqpacket_size_limit() {
        let bytes = std::vec![0; MAX_IDENTITY_MESSAGE_BYTES + 1];
        assert_eq!(
            IdentityChallenge::decode_packet(&bytes),
            Err(IdentityChannelError::Payload(
                PayloadCodecError::PayloadTooLarge
            ))
        );
    }

    #[test]
    fn identity_packet_encoder_reports_the_packet_limit() {
        struct OversizedPacket;

        impl Serialize for OversizedPacket {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut sequence =
                    serializer.serialize_seq(Some(MAX_IDENTITY_MESSAGE_BYTES + 1))?;
                for _ in 0..=MAX_IDENTITY_MESSAGE_BYTES {
                    sequence.serialize_element(&0u8)?;
                }
                sequence.end()
            }
        }

        let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
        assert_eq!(
            encode_packet(&OversizedPacket, &mut bytes),
            Err(IdentityChannelError::Payload(
                PayloadCodecError::PayloadTooLarge
            ))
        );
    }
}
