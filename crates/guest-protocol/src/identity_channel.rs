//! Challenge and account metadata exchanged with a prepared worker over its
//! dedicated socket. Payload correlation does not replace the host's kernel
//! credential and machine-membership checks.

use heapless::String;
use serde::{Deserialize, Serialize};

use crate::json::{decode_bounded_json, encode_bounded_json};
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

    pub(crate) fn encode_packet(self, output: &mut [u8]) -> Result<usize, IdentityChannelError> {
        encode_packet(&self, output)
    }

    pub(crate) fn decode_packet(bytes: &[u8]) -> Result<Self, IdentityChannelError> {
        let wire: WireIdentityChallenge = decode_packet(bytes)?;
        Ok(Self::new(wire.preparation_id, wire.nonce))
    }
}

/// UID and GID here are guest-reported values. The kernel supplies sender
/// credentials separately through SCM_CREDENTIALS.
///
/// Decoding checks the pending request and challenge. The guest helper remains
/// responsible for resolving the requested account to UID and GID.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuestIdentityReply {
    preparation_id: PreparationId,
    challenge: [u8; IDENTITY_CHALLENGE_BYTES],
    guest_user: String<MAX_GUEST_USER_NAME_BYTES>,
    guest_uid: u32,
    guest_gid: u32,
}

impl GuestIdentityReply {
    pub fn new(
        challenge: IdentityChallenge,
        request: &PrepareRequest<'_>,
        guest_user: &str,
        guest_uid: u32,
        guest_gid: u32,
    ) -> Result<Self, IdentityChannelError> {
        if guest_user != request.user() {
            return Err(IdentityChannelError::UserMismatch);
        }
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

    pub(crate) fn encode_packet(&self, output: &mut [u8]) -> Result<usize, IdentityChannelError> {
        encode_packet(self, output)
    }

    pub(crate) fn decode_for(
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
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityChannelError {
    Payload(PayloadCodecError),
    ChallengeMismatch,
    UserMismatch,
}

fn encode_packet<T: Serialize + ?Sized>(
    value: &T,
    output: &mut [u8],
) -> Result<usize, IdentityChannelError> {
    encode_bounded_json(value, output, MAX_IDENTITY_MESSAGE_BYTES)
        .map_err(IdentityChannelError::Payload)
}

fn decode_packet<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, IdentityChannelError> {
    decode_bounded_json(bytes, MAX_IDENTITY_MESSAGE_BYTES).map_err(IdentityChannelError::Payload)
}

#[cfg(test)]
#[path = "../tests/identity/channel.rs"]
mod tests;
