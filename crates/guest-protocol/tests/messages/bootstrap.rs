use super::*;
use crate::{ControlConnectionState, ControlFrameHeader, ProtocolVersion};

#[test]
fn manifest_registers_exactly_the_bootstrap_exchange() {
    assert_eq!(BOOTSTRAP_MANIFEST.minor, 0);
    assert_eq!(BOOTSTRAP_MANIFEST.limits.max_control_frame_bytes(), 1024);
    assert_eq!(BOOTSTRAP_MANIFEST.limits.max_descriptors(), 0);
    assert_eq!(BOOTSTRAP_MANIFEST.payload_bytes(), 992);
    assert_eq!(BOOTSTRAP_MANIFEST.messages.len(), 3);
    for schema in BOOTSTRAP_MANIFEST.messages {
        assert_eq!(BOOTSTRAP_MANIFEST.schema(schema.kind), Some(*schema));
        let header = ControlFrameHeader::new_with_version(
            ProtocolVersion {
                major: 7,
                minor: BOOTSTRAP_MANIFEST.minor,
            },
            schema.kind,
            1,
            BOOTSTRAP_MANIFEST.payload_bytes() as u32,
            0,
        )
        .unwrap();
        let request = ControlFrameHeader::new_with_version(
            header.protocol_version(),
            MessageKind::HelloRequest,
            1,
            0,
            0,
        )
        .unwrap();
        match schema.direction {
            Direction::HostToGuest => {
                ControlConnectionState::host()
                    .next_outbound(header)
                    .unwrap();
                ControlConnectionState::guest()
                    .next_inbound(header)
                    .unwrap();
            }
            Direction::GuestToHost => {
                ControlConnectionState::host()
                    .next_outbound(request)
                    .unwrap()
                    .next_inbound(header)
                    .unwrap();
                ControlConnectionState::guest()
                    .next_inbound(request)
                    .unwrap()
                    .next_outbound(header)
                    .unwrap();
            }
        }
    }
    assert!(BOOTSTRAP_MANIFEST.schema(MessageKind::Exit).is_none());
}
