use lasper_guest_protocol::{ControlLimits, MessageKind, CONTROL_FRAME_HEADER_BYTES};
use lasper_guest_runtime::transport::{IoBudget, TransportError};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::task::Poll;
use std::time::Duration;

use super::support::{assert_closed, established, header, small_send_buffer};

fn frame() -> Vec<u8> {
    let mut bytes = header(MessageKind::PrepareRequest, 7, 0).encode().to_vec();
    bytes.extend_from_slice(b"payload");
    bytes
}

#[test]
fn every_frame_split_preserves_receive_progress() {
    let bytes = frame();
    for split in 1..bytes.len() {
        let (guest, mut peer) = established(ControlLimits::CURRENT);
        let mut reader = guest.begin_receive().unwrap();
        peer.write_all(&bytes[..split]).unwrap();
        if split < CONTROL_FRAME_HEADER_BYTES {
            assert_eq!(reader.advance(), Ok(Poll::Pending));
            peer.write_all(&bytes[split..]).unwrap();
            assert!(reader.advance().unwrap().is_ready());
            let mut body = reader.into_payload([0; 7]).unwrap();
            assert_eq!(body.advance(), Ok(Poll::Ready(())));
            assert_eq!(body.finish().unwrap().payload(), b"payload");
        } else {
            assert!(reader.advance().unwrap().is_ready());
            let mut body = reader.into_payload([0; 7]).unwrap();
            assert_eq!(body.advance(), Ok(Poll::Pending));
            peer.write_all(&bytes[split..]).unwrap();
            assert_eq!(body.advance(), Ok(Poll::Ready(())));
            assert_eq!(body.finish().unwrap().payload(), b"payload");
        }
        assert!(guest.can_continue(), "split {split}");
    }
}

#[test]
fn eof_at_every_frame_offset_poisoning_is_not_a_complete_request() {
    let bytes = frame();
    for offset in 0..bytes.len() {
        let (guest, mut peer) = established(ControlLimits::CURRENT);
        peer.write_all(&bytes[..offset]).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reader = guest.begin_receive().unwrap();
        if offset < CONTROL_FRAME_HEADER_BYTES {
            assert_eq!(reader.advance(), Err(TransportError::UnexpectedEof));
            drop(reader);
        } else {
            assert!(reader.advance().unwrap().is_ready());
            let mut body = reader.into_payload([0; 7]).unwrap();
            assert_eq!(body.advance(), Err(TransportError::UnexpectedEof));
            drop(body);
        }
        assert!(!guest.can_continue(), "offset {offset}");
        assert_closed(peer);
    }
}

fn send_right(peer: &mut UnixStream, byte: u8, descriptor: &UnixStream) {
    send_rights(peer, byte, &[descriptor.as_raw_fd()]);
}

fn send_rights(peer: &mut UnixStream, byte: u8, descriptors: &[i32]) {
    let mut control = [0usize; 32];
    let mut iovec = libc::iovec {
        iov_base: std::ptr::from_ref(&byte).cast_mut().cast(),
        iov_len: 1,
    };
    // SAFETY: The aligned control buffer holds this SCM_RIGHTS record. All
    // pointers remain live until sendmsg returns; the kernel duplicates the FD.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(size_of_val(descriptors) as u32) as usize;
        assert!(message.msg_controllen <= size_of_val(&control));
        let record = libc::CMSG_FIRSTHDR(&message);
        (*record).cmsg_level = libc::SOL_SOCKET;
        (*record).cmsg_type = libc::SCM_RIGHTS;
        (*record).cmsg_len = libc::CMSG_LEN(size_of_val(descriptors) as u32) as usize;
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr(),
            libc::CMSG_DATA(record).cast::<i32>(),
            descriptors.len(),
        );
        assert_eq!(
            libc::sendmsg(peer.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
            1
        );
    }
}

#[test]
fn kernel_truncated_rights_close_all_delivered_descriptors() {
    let (guest, mut peer) = established(ControlLimits::CURRENT);
    let pairs: Vec<_> = (0..16).map(|_| UnixStream::pair().unwrap()).collect();
    let descriptors: Vec<_> = pairs.iter().map(|(export, _)| export.as_raw_fd()).collect();
    let bytes = frame();
    send_rights(&mut peer, bytes[0], &descriptors);
    peer.write_all(&bytes[1..]).unwrap();
    let witnesses: Vec<_> = pairs.into_iter().map(|(_, witness)| witness).collect();
    let mut reader = guest.begin_receive().unwrap();
    assert_eq!(reader.advance(), Err(TransportError::Truncated));
    drop(reader);
    assert!(!guest.can_continue());
    assert_closed(peer);
    // Linux closes rights that did not fit the ancillary buffer; the runtime
    // owns and closes every FD that did fit before returning Truncated.
    for mut witness in witnesses {
        witness
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(witness.read(&mut [0]).unwrap(), 0);
    }
}

#[test]
fn anchor_only_send_and_backpressure_transfer_rights_once() {
    let (guest, peer) = established(ControlLimits::CURRENT);
    small_send_buffer(guest.as_raw_fd());
    let (export, mut witness) = UnixStream::pair().unwrap();
    let payload = vec![b'x'; 256 * 1024];
    let frame_header = header(MessageKind::PrepareReady, payload.len(), 1);
    let mut sender = guest
        .begin_send(frame_header, &payload, &[export.as_raw_fd()])
        .unwrap();
    drop(export);
    assert_eq!(
        sender.advance_with_budget(&mut IoBudget::with_bytes(1)),
        Ok(Poll::Pending)
    );
    assert_eq!(sender.advance(), Ok(Poll::Pending));
    assert_eq!(sender.advance(), Ok(Poll::Pending));
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let receiver = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut descriptors = Vec::new();
        while bytes.len() < CONTROL_FRAME_HEADER_BYTES + 256 * 1024 {
            let mut chunk = [0; 4096];
            let mut control = [0usize; 16];
            let mut vector = libc::iovec {
                iov_base: chunk.as_mut_ptr().cast(),
                iov_len: chunk.len(),
            };
            // SAFETY: recvmsg borrows live writable data and aligned control
            // buffers. Each returned SCM_RIGHTS FD acquires exactly one owner.
            unsafe {
                let mut message: libc::msghdr = std::mem::zeroed();
                message.msg_iov = &mut vector;
                message.msg_iovlen = 1;
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = size_of_val(&control);
                let count = libc::recvmsg(peer.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
                assert!(count > 0);
                assert_eq!(message.msg_flags & libc::MSG_CTRUNC, 0);
                let mut record = libc::CMSG_FIRSTHDR(&message);
                while !record.is_null() {
                    assert_eq!((*record).cmsg_level, libc::SOL_SOCKET);
                    assert_eq!((*record).cmsg_type, libc::SCM_RIGHTS);
                    assert!(bytes.is_empty(), "rights repeated after the anchor");
                    let slots =
                        ((*record).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<i32>();
                    for index in 0..slots {
                        descriptors.push(OwnedFd::from_raw_fd(
                            libc::CMSG_DATA(record)
                                .cast::<i32>()
                                .add(index)
                                .read_unaligned(),
                        ));
                    }
                    record = libc::CMSG_NXTHDR(&message, record);
                }
                bytes.extend_from_slice(&chunk[..count as usize]);
            }
        }
        (bytes, descriptors)
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sender.advance().unwrap().is_pending() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    let (bytes, descriptors) = receiver.join().unwrap();
    assert_eq!(&bytes[..CONTROL_FRAME_HEADER_BYTES], &frame_header.encode());
    assert_eq!(&bytes[CONTROL_FRAME_HEADER_BYTES..], payload);
    assert_eq!(descriptors.len(), 1);
    drop(descriptors);
    witness
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(witness.read(&mut [0]).unwrap(), 0);
}

#[test]
fn unexpected_rights_at_every_offset_are_closed() {
    let bytes = frame();
    for offset in 0..bytes.len() {
        let (guest, mut peer) = established(ControlLimits::CURRENT);
        let (export, mut witness) = UnixStream::pair().unwrap();
        witness
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = guest.begin_receive().unwrap();
        peer.write_all(&bytes[..offset]).unwrap();
        if offset < CONTROL_FRAME_HEADER_BYTES {
            assert_eq!(reader.advance(), Ok(Poll::Pending));
            send_right(&mut peer, bytes[offset], &export);
            peer.write_all(&bytes[offset + 1..]).unwrap();
            drop(export);
            assert!(reader.advance().is_err(), "offset {offset}");
            drop(reader);
        } else {
            assert!(reader.advance().unwrap().is_ready());
            let mut body = reader.into_payload([0; 7]).unwrap();
            assert_eq!(body.advance(), Ok(Poll::Pending));
            send_right(&mut peer, bytes[offset], &export);
            peer.write_all(&bytes[offset + 1..]).unwrap();
            drop(export);
            assert_eq!(body.advance(), Err(TransportError::UnexpectedDescriptors));
            drop(body);
        }
        assert!(!guest.can_continue(), "offset {offset}");
        assert_closed(peer);
        assert_eq!(witness.read(&mut [0]).unwrap(), 0, "offset {offset}");
    }
}
