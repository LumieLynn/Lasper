use super::*;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn credentials_record(control: &mut AncillaryBuffer, offset: usize, pid: libc::pid_t) -> usize {
    let bytes = unsafe { libc::CMSG_SPACE(size_of::<libc::ucred>() as _) as usize };
    assert!(offset + bytes <= size_of::<AncillaryBuffer>());
    // SAFETY: The local control allocation holds the complete header and ucred.
    unsafe {
        let header = control
            .0
            .as_mut_ptr()
            .cast::<u8>()
            .add(offset)
            .cast::<libc::cmsghdr>();
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::ucred>() as _) as _;
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_CREDENTIALS;
        libc::CMSG_DATA(header)
            .cast::<libc::ucred>()
            .write_unaligned(libc::ucred {
                pid,
                uid: 1000,
                gid: 1000,
            });
    }
    bytes
}

fn fd_record(control: &mut AncillaryBuffer, offset: usize, kind: i32, fds: &[RawFd]) -> usize {
    let data_bytes = std::mem::size_of_val(fds);
    let bytes = unsafe { libc::CMSG_SPACE(data_bytes as _) as usize };
    assert!(offset + bytes <= size_of::<AncillaryBuffer>());
    // SAFETY: The local allocation contains this complete, aligned record.
    unsafe {
        let header = control
            .0
            .as_mut_ptr()
            .cast::<u8>()
            .add(offset)
            .cast::<libc::cmsghdr>();
        (*header).cmsg_len = libc::CMSG_LEN(data_bytes as _) as _;
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = kind;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(header),
            data_bytes,
        );
    }
    bytes
}

fn assert_closed(mut peer: UnixStream) {
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[test]
fn credentials_must_be_unique_complete_and_have_a_visible_pid() {
    let mut control = AncillaryBuffer([0; 16]);
    // SAFETY: Only msg_controllen is used by this parser.
    let mut message: libc::msghdr = unsafe { zeroed() };
    assert!(parse_credentials(&message, &control, false).is_err());
    message.msg_controllen = credentials_record(&mut control, 0, 10) as _;
    assert_eq!(
        parse_credentials(&message, &control, false).unwrap().pid(),
        10
    );
    assert!(parse_credentials(&message, &control, false)
        .unwrap()
        .pidfd()
        .is_none());
    assert!(parse_credentials(&message, &control, true).is_err());
    let offset: usize = message.msg_controllen as _;
    message.msg_controllen = (offset + credentials_record(&mut control, offset, 20)) as _;
    assert!(parse_credentials(&message, &control, false).is_err());
    message.msg_controllen = credentials_record(&mut control, 0, 0) as _;
    assert!(parse_credentials(&message, &control, false).is_err());
    message.msg_controllen -= 8;
    assert!(parse_credentials(&message, &control, false).is_err());
}

#[test]
fn empty_rights_are_rejected_even_with_valid_credentials() {
    let mut control = AncillaryBuffer([0; 16]);
    let offset = credentials_record(&mut control, 0, 10);
    // SAFETY: Both records fit the local ancillary allocation.
    let mut message: libc::msghdr = unsafe { zeroed() };
    unsafe {
        let header = control
            .0
            .as_mut_ptr()
            .cast::<u8>()
            .add(offset)
            .cast::<libc::cmsghdr>();
        (*header).cmsg_len = libc::CMSG_LEN(0) as _;
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        message.msg_controllen = (offset + libc::CMSG_SPACE(0) as usize) as _;
    }
    assert!(parse_credentials(&message, &control, false).is_err());
}

#[test]
fn expected_pidfd_is_owned_until_credentials_are_dropped() {
    let (received, peer) = UnixStream::pair().unwrap();
    let mut control = AncillaryBuffer([0; 16]);
    // SAFETY: The parser only reads msg_controllen from the message header.
    let mut message: libc::msghdr = unsafe { zeroed() };
    let offset = credentials_record(&mut control, 0, 10);
    message.msg_controllen =
        (offset + fd_record(&mut control, offset, SCM_PIDFD, &[received.into_raw_fd()])) as _;
    let credentials = parse_credentials(&message, &control, true).unwrap();
    assert!(credentials.pidfd().is_some());
    drop(credentials);
    assert_closed(peer);
}

#[test]
fn unexpected_or_malformed_pidfd_records_close_every_descriptor() {
    for (expected, duplicate, slots) in [(false, false, 1), (true, true, 1), (true, false, 2)] {
        let mut control = AncillaryBuffer([0; 16]);
        let mut peers = Vec::new();
        let mut fds = Vec::new();
        for _ in 0..slots {
            let (received, peer) = UnixStream::pair().unwrap();
            peers.push(peer);
            fds.push(received.into_raw_fd());
        }
        // SAFETY: The parser only reads msg_controllen from the message header.
        let mut message: libc::msghdr = unsafe { zeroed() };
        let mut offset = credentials_record(&mut control, 0, 10);
        if duplicate {
            let (received, peer) = UnixStream::pair().unwrap();
            peers.push(peer);
            offset += fd_record(&mut control, offset, SCM_PIDFD, &[received.into_raw_fd()]);
        }
        message.msg_controllen = (offset + fd_record(&mut control, offset, SCM_PIDFD, &fds)) as _;
        assert!(parse_credentials(&message, &control, expected).is_err());
        for peer in peers {
            assert_closed(peer);
        }
    }
}

#[test]
fn negative_pidfd_and_unknown_control_still_close_later_rights() {
    let (received, peer) = UnixStream::pair().unwrap();
    let mut control = AncillaryBuffer([0; 16]);
    let mut offset = credentials_record(&mut control, 0, 10);
    offset += fd_record(&mut control, offset, SCM_PIDFD, &[-libc::ESRCH]);
    offset += fd_record(&mut control, offset, 0x7fff, &[]);
    // SAFETY: The parser only reads msg_controllen from the message header.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_controllen = (offset
        + fd_record(
            &mut control,
            offset,
            libc::SCM_RIGHTS,
            &[received.into_raw_fd()],
        )) as _;
    assert!(parse_credentials(&message, &control, true).is_err());
    assert_closed(peer);
}
