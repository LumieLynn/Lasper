//! The worker identity channel uses its own packet schema. Control-stream frame
//! limits and feature gates do not apply to these packets.

use crate::profile::{IdentitySchemaId, ProtocolProfile};
use crate::{
    GuestIdentityReply, IdentityChallenge, IdentityChannelError, PayloadCodecError, PrepareRequest,
};

impl ProtocolProfile {
    pub fn encode_identity_challenge(
        &self,
        challenge: IdentityChallenge,
        output: &mut [u8],
    ) -> Result<usize, IdentityChannelError> {
        match self.identity_schema() {
            IdentitySchemaId::IdentityV1 => challenge.encode_packet(output),
        }
    }

    pub fn decode_identity_challenge(
        &self,
        packet: &[u8],
    ) -> Result<IdentityChallenge, IdentityChannelError> {
        match self.identity_schema() {
            IdentitySchemaId::IdentityV1 => IdentityChallenge::decode_packet(packet),
        }
    }

    pub fn encode_identity_reply(
        &self,
        reply: &GuestIdentityReply,
        output: &mut [u8],
    ) -> Result<usize, IdentityChannelError> {
        self.validate_identity_user(reply)?;
        match self.identity_schema() {
            IdentitySchemaId::IdentityV1 => reply.encode_packet(output),
        }
    }

    /// Checks correlation and account metadata. The host must independently
    /// verify kernel credentials, machine membership and UID/GID mappings.
    pub fn decode_identity_reply(
        &self,
        packet: &[u8],
        challenge: IdentityChallenge,
        request: &PrepareRequest<'_>,
    ) -> Result<GuestIdentityReply, IdentityChannelError> {
        let reply = match self.identity_schema() {
            IdentitySchemaId::IdentityV1 => {
                GuestIdentityReply::decode_for(packet, challenge, request)
            }
        }?;
        self.validate_identity_user(&reply)?;
        Ok(reply)
    }

    fn validate_identity_user(
        &self,
        reply: &GuestIdentityReply,
    ) -> Result<(), IdentityChannelError> {
        if reply.guest_user().len() > self.capacities().user_name_bytes {
            return Err(IdentityChannelError::Payload(
                PayloadCodecError::InvalidRequest,
            ));
        }
        Ok(())
    }
}
