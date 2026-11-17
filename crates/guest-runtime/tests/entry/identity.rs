use super::*;

#[test]
fn advertised_identity_is_the_shared_protocol_identity() {
    let frame = identity_frame();
    assert_eq!(
        lasper_guest_protocol::RuntimeIdentity::decode(&frame),
        Ok(RUNTIME_IDENTITY)
    );
}
