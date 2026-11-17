use lasper_guest_protocol::{
    BrokerId, ControlLimitError, ControlLimits, FeatureSet, HelloRequest, HelloResponse,
    MessageKind, PrepareFrameError,
};
use lasper_guest_runtime::session::{PrepareBuffer, PrepareStorage, PrepareStorageError};
use std::io::Write;
use std::task::Poll;

use super::support::{established, header};

fn limits(value: ControlLimits) -> lasper_guest_protocol::EnforcedControlLimits {
    HelloResponse::accept(
        HelloRequest::new(FeatureSet::EMPTY, value).unwrap(),
        lasper_guest_protocol::HelloOffer::local(value),
        BrokerId::new([1; 16]),
    )
    .unwrap()
    .1
    .limits()
}

#[test]
fn prepare_storage_rejects_wrong_envelopes_and_limits_before_allocating() {
    let broker = BrokerId::new([1; 16]);
    let current = limits(ControlLimits::CURRENT);
    assert!(matches!(
        PrepareBuffer::new(header(MessageKind::Start, 64, 0), broker, current),
        Err(PrepareStorageError::Frame(
            PrepareFrameError::UnexpectedMessageKind { .. }
        ))
    ));
    assert!(matches!(
        PrepareBuffer::new(header(MessageKind::PrepareRequest, 64, 1), broker, current),
        Err(PrepareStorageError::Frame(
            PrepareFrameError::UnexpectedDescriptorCount { .. }
        ))
    ));
    let small = limits(ControlLimits::new(4096, 0).unwrap());
    assert!(matches!(
        PrepareBuffer::new(header(MessageKind::PrepareRequest, 4096, 0), broker, small),
        Err(PrepareStorageError::Limits(
            ControlLimitError::FrameTooLarge { .. }
        ))
    ));
}

#[test]
fn freezing_requires_the_header_that_allocated_the_storage() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    let broker = BrokerId::new([1; 16]);
    let expected = header(MessageKind::PrepareRequest, 2, 0);
    let actual = lasper_guest_protocol::ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        expected.request_id() + 1,
        2,
        0,
    )
    .unwrap();
    peer.write_all(&actual.encode()).unwrap();
    peer.write_all(b"{}").unwrap();
    let buffer = PrepareBuffer::new(expected, broker, limits(ControlLimits::CURRENT)).unwrap();
    let mut reader = guest.begin_receive().unwrap();
    assert_eq!(reader.advance(), Ok(Poll::Ready(actual)));
    let mut receiver = reader.into_payload(buffer).unwrap();
    assert_eq!(receiver.advance(), Ok(Poll::Ready(())));
    assert!(matches!(
        PrepareStorage::from_received(receiver.finish().unwrap()),
        Err(PrepareStorageError::HeaderChanged)
    ));
}
