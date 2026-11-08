use super::*;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;

const TEST_CONTROL_BYTES: usize = 128;

fn assert_peer_observes_close(mut peer: StdUnixStream) {
    peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

fn rights_message<const BYTES: usize>(
    control: &mut AncillaryBuffer<BYTES>,
    descriptors: &[RawFd],
) -> libc::msghdr {
    rights_records_message(control, descriptors, &[descriptors.len()])
}

fn rights_records_message<const BYTES: usize>(
    control: &mut AncillaryBuffer<BYTES>,
    descriptors: &[RawFd],
    records: &[usize],
) -> libc::msghdr {
    // Only the control fields are used until a caller installs a data buffer.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_control = control.0.as_mut_ptr().cast();
    let mut offset = 0;
    let mut first_fd = 0;
    for &count in records {
        let descriptors = &descriptors[first_fd..first_fd + count];
        let data_bytes = std::mem::size_of_val(descriptors);
        let record_bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) } as usize;
        assert!(offset + record_bytes <= control.0.len());
        // SAFETY: This aligned header and its FD array fit the control buffer.
        // The source slice contains exactly count live descriptors.
        unsafe {
            let header = control
                .0
                .as_mut_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<libc::cmsghdr>();
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(data_bytes as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                data_bytes,
            );
        }
        first_fd += count;
        offset += record_bytes;
    }
    assert_eq!(first_fd, descriptors.len());
    message.msg_controllen = offset as _;
    message
}

#[test]
fn empty_rights_record_is_rejected() {
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let message = rights_message(&mut control, &[]);

    let error = decode_ancillary(&message, &control.0).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn unexpected_pidfd_is_closed_even_on_truncation() {
    for flags in [0, libc::MSG_CTRUNC] {
        let (received, peer) = StdUnixStream::pair().unwrap();
        let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
        let mut message = rights_message(&mut control, &[received.into_raw_fd()]);
        // SAFETY: rights_message installed this complete record in control.
        unsafe {
            (*message.msg_control.cast::<libc::cmsghdr>()).cmsg_type = SCM_PIDFD;
        }
        message.msg_flags = flags;
        assert!(decode_ancillary(&message, &control.0).is_err());
        assert_peer_observes_close(peer);
    }
}

#[test]
fn truncated_control_closes_descriptors_before_returning() {
    let (received, peer) = StdUnixStream::pair().unwrap();
    let fd = received.into_raw_fd();
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let mut message = rights_message(&mut control, &[fd]);
    message.msg_flags = libc::MSG_CTRUNC;

    let error = decode_ancillary(&message, &control.0).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_peer_observes_close(peer);
}

#[test]
fn oversized_control_length_closes_descriptors_before_returning() {
    let (received, peer) = StdUnixStream::pair().unwrap();
    let fd = received.into_raw_fd();
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    let mut message = rights_message(&mut control, &[fd]);
    message.msg_controllen = (control.0.len() + 1) as _;

    let error = decode_ancillary(&message, &control.0).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_peer_observes_close(peer);
}

#[test]
fn incomplete_record_closes_all_complete_descriptor_slots() {
    for kind in [libc::SCM_RIGHTS, SCM_PIDFD] {
        for flags in [0, libc::MSG_CTRUNC] {
            for trailing_bytes in 0..size_of::<RawFd>() {
                for advertised_bytes in [3 * size_of::<RawFd>(), usize::MAX - 16] {
                    let (first, first_peer) = StdUnixStream::pair().unwrap();
                    let (second, second_peer) = StdUnixStream::pair().unwrap();
                    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
                    let mut message =
                        rights_message(&mut control, &[first.into_raw_fd(), second.into_raw_fd()]);
                    message.msg_controllen =
                        (unsafe { libc::CMSG_LEN((2 * size_of::<RawFd>()) as _) } as usize
                            + trailing_bytes) as _;
                    message.msg_flags = flags;
                    // SAFETY: The helper installed a complete header. Only its
                    // advertised length and kind change; the FD bytes stay intact.
                    unsafe {
                        let header = &mut *message.msg_control.cast::<libc::cmsghdr>();
                        header.cmsg_len = (libc::CMSG_LEN(0) as usize + advertised_bytes) as _;
                        header.cmsg_type = kind;
                    }

                    assert_eq!(
                        decode_ancillary(&message, &control.0).unwrap_err().kind(),
                        io::ErrorKind::InvalidData
                    );
                    assert_peer_observes_close(first_peer);
                    assert_peer_observes_close(second_peer);
                }
            }
        }
    }
}

#[test]
fn kernel_truncation_never_installs_unreported_rights() {
    let header_bytes = unsafe { libc::CMSG_LEN(0) } as usize;
    let rights_bytes = unsafe { libc::CMSG_SPACE((5 * size_of::<RawFd>()) as _) } as usize;
    let credential_bytes = unsafe { libc::CMSG_SPACE(size_of::<libc::ucred>() as _) } as usize;
    for pass_credentials in [false, true] {
        for records in [&[5_usize][..], &[1, 1, 3][..]] {
            // Include every byte boundary: missing/partial headers, partial FD
            // slots, and rights after credentials have consumed the available space.
            for capacity in 0..=credential_bytes + rights_bytes {
                let (sender, receiver) = StdUnixStream::pair().unwrap();
                let enabled = i32::from(pass_credentials);
                // SAFETY: setsockopt reads this live integer for the call.
                assert_eq!(
                    unsafe {
                        libc::setsockopt(
                            receiver.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_PASSCRED,
                            (&enabled as *const i32).cast(),
                            size_of::<i32>() as _,
                        )
                    },
                    0
                );
                let (transferred, peers): (Vec<_>, Vec<_>) =
                    (0..5).map(|_| StdUnixStream::pair().unwrap()).unzip();
                let raw = transferred
                    .iter()
                    .map(AsRawFd::as_raw_fd)
                    .collect::<Vec<_>>();
                let mut control = AncillaryBuffer([0; TEST_CONTROL_BYTES]);
                let mut message = rights_records_message(&mut control, &raw, records);
                let mut byte = [1_u8];
                let mut vector = libc::iovec {
                    iov_base: byte.as_mut_ptr().cast(),
                    iov_len: 1,
                };
                message.msg_iov = &mut vector;
                message.msg_iovlen = 1;
                // SAFETY: All message pointers refer to live buffers sized above.
                assert_eq!(
                    unsafe { libc::sendmsg(sender.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
                    1
                );
                drop(transferred);

                let mut received_control = AncillaryBuffer([0; TEST_CONTROL_BYTES]);
                assert!(capacity <= received_control.0.len());
                message.msg_control = received_control.0.as_mut_ptr().cast();
                message.msg_controllen = capacity as _;
                // SAFETY: recvmsg may write only capacity bytes of the live control allocation.
                assert_eq!(
                    unsafe {
                        libc::recvmsg(receiver.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC)
                    },
                    1
                );
                let control_bytes: usize = message.msg_controllen as _;
                // Take ownership before assertions so failures cannot leak received rights.
                let (descriptors, _) = collect_descriptors(&message, &received_control.0).unwrap();
                assert!(control_bytes <= capacity);
                if capacity < header_bytes + size_of::<RawFd>() {
                    assert!(descriptors.is_empty());
                    assert_ne!(message.msg_flags & libc::MSG_CTRUNC, 0);
                }
                if capacity == credential_bytes + rights_bytes {
                    assert_eq!(message.msg_flags & libc::MSG_CTRUNC, 0);
                    assert_eq!(descriptors.len(), 5);
                }
                drop(descriptors);
                for peer in peers {
                    assert_peer_observes_close(peer);
                }
            }
        }
    }
}

#[test]
fn incomplete_later_record_also_releases_preceding_rights() {
    let (first, first_peer) = StdUnixStream::pair().unwrap();
    let (second, second_peer) = StdUnixStream::pair().unwrap();
    let mut control = AncillaryBuffer([0; TEST_CONTROL_BYTES]);
    let mut message = rights_message(&mut control, &[first.into_raw_fd()]);
    let offset: usize = message.msg_controllen as _;
    // SAFETY: The second header and its one delivered FD fit the allocation.
    // Its length claims a second FD beyond the supplied control bytes.
    unsafe {
        let header = control
            .0
            .as_mut_ptr()
            .cast::<u8>()
            .add(offset)
            .cast::<libc::cmsghdr>();
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((2 * size_of::<RawFd>()) as _) as _;
        libc::CMSG_DATA(header)
            .cast::<RawFd>()
            .write_unaligned(second.into_raw_fd());
        message.msg_controllen = (offset + libc::CMSG_LEN(size_of::<RawFd>() as _) as usize) as _;
    }

    assert_eq!(
        decode_ancillary(&message, &control.0).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_peer_observes_close(first_peer);
    assert_peer_observes_close(second_peer);
}
