extern crate std;

use super::*;
use crate::{ExecutionKind, GuestProcess, ProcessEnvironment};

#[test]
fn identity_packets_use_the_identity_capacity_and_preserve_buffer_errors() {
    let value =
        std::string::String::from_utf8(std::vec![b'x'; MAX_IDENTITY_MESSAGE_BYTES - 2]).unwrap();
    let mut packet = std::vec![0; MAX_IDENTITY_MESSAGE_BYTES];
    let length = encode_packet(value.as_str(), &mut packet).unwrap();
    assert_eq!(length, MAX_IDENTITY_MESSAGE_BYTES);
    let decoded = decode_packet::<String<MAX_IDENTITY_MESSAGE_BYTES>>(&packet).unwrap();
    assert_eq!(decoded.as_str(), value);
    assert_eq!(
        encode_packet(
            value.as_str(),
            &mut packet[..MAX_IDENTITY_MESSAGE_BYTES - 1]
        ),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::OutputTooSmall
        ))
    );
    let mut larger = value;
    larger.push('x');
    assert_eq!(
        encode_packet(larger.as_str(), &mut packet),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::PayloadTooLarge
        ))
    );
    packet.push(b' ');
    assert_eq!(
        IdentityChallenge::decode_packet(&packet),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::PayloadTooLarge
        ))
    );
}

#[test]
fn identity_packets_reject_unknown_duplicate_and_trailing_data() {
    let challenge =
        IdentityChallenge::new(PreparationId::new([1; 16]), [2; IDENTITY_CHALLENGE_BYTES]);
    let mut packet = [0; 1024];
    let length = challenge.encode_packet(&mut packet).unwrap();
    let mut unknown = packet[..length - 1].to_vec();
    unknown.extend_from_slice(br#", "extra":0}"#);
    assert!(matches!(
        IdentityChallenge::decode_packet(&unknown),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::InvalidJson
        ))
    ));
    let mut duplicate = packet[..length - 1].to_vec();
    duplicate.push(b',');
    duplicate.extend_from_slice(&packet[1..length]);
    assert!(matches!(
        IdentityChallenge::decode_packet(&duplicate),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::InvalidJson
        ))
    ));
    let mut trailing = packet[..length].to_vec();
    trailing.extend_from_slice(b" {}");
    assert!(matches!(
        IdentityChallenge::decode_packet(&trailing),
        Err(IdentityChannelError::Payload(
            PayloadCodecError::InvalidJson
        ))
    ));
}

#[test]
fn identity_reply_is_bound_to_account_preparation_and_challenge() {
    let request = PrepareRequest::new(
        "alice",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        ProcessEnvironment::default(),
        None,
    )
    .unwrap();
    let challenge =
        IdentityChallenge::new(PreparationId::new([1; 16]), [2; IDENTITY_CHALLENGE_BYTES]);
    let mut packet = [0; 1024];
    let length = challenge.encode_packet(&mut packet).unwrap();
    assert_eq!(
        IdentityChallenge::decode_packet(&packet[..length]),
        Ok(challenge)
    );
    let reply = GuestIdentityReply::new(challenge, &request, "alice", 1000, 1001).unwrap();
    let length = reply.encode_packet(&mut packet).unwrap();
    assert_eq!(
        GuestIdentityReply::decode_for(&packet[..length], challenge, &request),
        Ok(reply)
    );
    let other_nonce =
        IdentityChallenge::new(challenge.preparation_id(), [3; IDENTITY_CHALLENGE_BYTES]);
    let other_preparation = IdentityChallenge::new(PreparationId::new([4; 16]), *challenge.nonce());
    for other in [other_nonce, other_preparation] {
        assert_eq!(
            GuestIdentityReply::decode_for(&packet[..length], other, &request),
            Err(IdentityChannelError::ChallengeMismatch)
        );
    }
    assert_eq!(
        GuestIdentityReply::new(challenge, &request, "bob", 1000, 1001),
        Err(IdentityChannelError::UserMismatch)
    );
    assert!(GuestIdentityReply::decode_for(&packet[..length - 1], challenge, &request).is_err());
}
