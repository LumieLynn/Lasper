use super::*;
use crate::MessageKind;

#[test]
fn negotiated_limits_reject_frames_and_descriptors_beyond_the_peer_offer() {
    let frame_limit = CONTROL_FRAME_HEADER_BYTES as u32 + 8;
    let limits = ControlLimits::new(frame_limit, 0).unwrap();
    let oversized = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        1,
        frame_limit - CONTROL_FRAME_HEADER_BYTES as u32 + 1,
        0,
    )
    .unwrap();
    let descriptor = crate::ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareReady,
        1,
        0,
        1,
    )
    .unwrap();

    assert_eq!(
        limits.validate_header(oversized),
        Err(ControlLimitError::FrameTooLarge {
            declared: frame_limit + 1,
            maximum: frame_limit,
        })
    );
    assert_eq!(
        limits.validate_header(descriptor),
        Err(ControlLimitError::TooManyDescriptors {
            declared: 1,
            maximum: 0,
        })
    );
}
