extern crate std;
use super::*;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;

fn rights_message<const BYTES: usize>(
    control: &mut AncillaryBuffer<BYTES>,
    descriptors: &[i32],
) -> MessageHeader {
    // The test fills the only control fields read by the parser below.
    let mut message: MessageHeader = unsafe { zeroed() };
    let data_bytes = core::mem::size_of_val(descriptors);
    message.control = control.0.as_mut_ptr().cast();
    message.control_length = cmsg_space(data_bytes);
    let header = message.control.cast::<ControlMessageHeader>();
    // The test buffer is aligned and sized for the supplied descriptors.
    unsafe {
        (*header).length = cmsg_len(data_bytes);
        (*header).level = SOL_SOCKET;
        (*header).kind = SCM_RIGHTS;
        ptr::copy_nonoverlapping(
            descriptors.as_ptr().cast::<u8>(),
            cmsg_data(header),
            data_bytes,
        );
    }
    message
}

fn assert_peer_observes_close(mut peer: UnixStream) {
    peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

#[test]
fn empty_rights_record_is_rejected() {
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let message = rights_message(&mut control, &[]);

    assert!(matches!(
        decode_ancillary(&message, &control.0),
        Err(TransportError::UnexpectedDescriptors)
    ));
}

#[test]
fn unexpected_pidfd_is_closed_even_on_truncation() {
    for flags in [0, MSG_CTRUNC] {
        let (received, peer) = UnixStream::pair().unwrap();
        let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
        let mut message = rights_message(&mut control, &[received.into_raw_fd()]);
        // SAFETY: rights_message installed this complete record in control.
        unsafe {
            (*message.control.cast::<ControlMessageHeader>()).kind = SCM_PIDFD;
        }
        message.flags = flags;
        assert!(decode_ancillary(&message, &control.0).is_err());
        assert_peer_observes_close(peer);
    }
}

#[test]
fn truncated_control_closes_descriptors_before_returning() {
    let (received, peer) = UnixStream::pair().unwrap();
    let fd = received.into_raw_fd();
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let mut message = rights_message(&mut control, &[fd]);
    message.flags = MSG_CTRUNC;

    assert!(matches!(
        decode_ancillary(&message, &control.0),
        Err(TransportError::Truncated)
    ));
    assert_peer_observes_close(peer);
}

#[test]
fn oversized_control_length_closes_descriptors_before_returning() {
    let (received, peer) = UnixStream::pair().unwrap();
    let fd = received.into_raw_fd();
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let mut message = rights_message(&mut control, &[fd]);
    message.control_length = control.0.len() + 1;

    assert!(matches!(
        decode_ancillary(&message, &control.0),
        Err(TransportError::Truncated)
    ));
    assert_peer_observes_close(peer);
}

#[test]
fn incomplete_record_closes_all_complete_descriptor_slots() {
    for kind in [SCM_RIGHTS, SCM_PIDFD] {
        for flags in [0, MSG_CTRUNC] {
            for trailing_bytes in 0..size_of::<i32>() {
                for advertised_bytes in [3 * size_of::<i32>(), usize::MAX - 16] {
                    let (first, first_peer) = UnixStream::pair().unwrap();
                    let (second, second_peer) = UnixStream::pair().unwrap();
                    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
                    let mut message =
                        rights_message(&mut control, &[first.into_raw_fd(), second.into_raw_fd()]);
                    message.control_length = cmsg_len(2 * size_of::<i32>()) + trailing_bytes;
                    message.flags = flags;
                    // SAFETY: The helper installed a complete header. Only its
                    // advertised length and kind change; the FD bytes stay intact.
                    unsafe {
                        let header = &mut *message.control.cast::<ControlMessageHeader>();
                        header.length = cmsg_len(advertised_bytes);
                        header.kind = kind;
                    }

                    assert!(matches!(
                        decode_ancillary(&message, &control.0),
                        Err(TransportError::Truncated)
                    ));
                    assert_peer_observes_close(first_peer);
                    assert_peer_observes_close(second_peer);
                }
            }
        }
    }
}
