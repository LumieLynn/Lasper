use super::*;

const TEST_IDENTITY: RuntimeIdentity = RuntimeIdentity::current(Architecture::X86_64);

#[test]
fn current_identity_round_trips() {
    let encoded = TEST_IDENTITY.encode();

    assert_eq!(RuntimeIdentity::decode(&encoded), Ok(TEST_IDENTITY));
}

#[test]
fn reserved_bytes_are_rejected() {
    let mut encoded = TEST_IDENTITY.encode();
    encoded[31] = 1;

    assert_eq!(
        RuntimeIdentity::decode(&encoded),
        Err(IdentityDecodeError::ReservedBytesSet)
    );
}

#[test]
fn unknown_architecture_is_not_silently_accepted() {
    let mut encoded = TEST_IDENTITY.encode();
    encoded[14..16].copy_from_slice(&u16::MAX.to_le_bytes());

    assert_eq!(
        RuntimeIdentity::decode(&encoded),
        Err(IdentityDecodeError::UnknownArchitecture)
    );
}
