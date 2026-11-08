use lasper_guest_protocol::{
    GuestIdentityReply, IdentityChannelError, PrepareReadyFrame, PrepareRequest,
    IDENTITY_CHALLENGE_BYTES, MAX_IDENTITY_MESSAGE_BYTES,
};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::unix::AsyncFd;

use crate::SCM_PIDFD;

const CHALLENGE_BUFFER_BYTES: usize = 256;

#[repr(C, align(16))]
struct AncillaryBuffer([usize; 16]);

/// The sender's kernel credentials in the receiver's namespaces. A pidfd, when
/// available, comes from the same packet; opening one later by PID would race
/// with process exit and PID reuse. Machine membership is not verified here.
#[derive(Debug)]
pub struct IdentityPeerCredentials {
    pid: i32,
    uid: u32,
    gid: u32,
    pidfd: Option<OwnedFd>,
}

impl IdentityPeerCredentials {
    pub const fn pid(&self) -> i32 {
        self.pid
    }

    pub const fn uid(&self) -> u32 {
        self.uid
    }

    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// This handle identifies the sender even after exit; it does not reserve
    /// the numeric PID. Check for exit around acquiring proc/namespace handles.
    ///
    /// When SO_PASSPIDFD is unavailable, IdentityWorker still checks the sender
    /// against the live worker pidfd assigned in PrepareReady.
    pub fn pidfd(&self) -> Option<BorrowedFd<'_>> {
        self.pidfd.as_ref().map(AsFd::as_fd)
    }
}

/// Cancelling an observation shuts down this channel. A late worker reply must
/// never be consumed by a subsequent observation of the same preparation.
pub struct UnixIdentityChannel {
    socket: AsyncFd<OwnedFd>,
    ready: PrepareReadyFrame,
    poisoned: bool,
    pass_pidfd: bool,
}

impl UnixIdentityChannel {
    /// Takes the identity endpoint delivered with `ready` and binds every
    /// observation to that preparation. SO_PASSPIDFD is enabled when supported.
    pub fn new(socket: OwnedFd, ready: PrepareReadyFrame) -> io::Result<Self> {
        verify_socket(socket.as_raw_fd())?;
        set_nonblocking(socket.as_raw_fd())?;
        set_socket_option(socket.as_raw_fd(), libc::SO_PASSCRED, 1)?;
        let pass_pidfd = enable_passpidfd(socket.as_raw_fd())?;
        // SAFETY: OwnedFd keeps the descriptor open and stable; this type never
        // exposes a way to replace or extract it while registered.
        let socket = unsafe { AsyncFd::register(socket)? };
        Ok(Self {
            socket,
            ready,
            poisoned: false,
            pass_pidfd,
        })
    }

    pub const fn can_continue(&self) -> bool {
        !self.poisoned
    }

    pub(super) fn invalidate(&mut self) {
        self.poisoned = true;
        // SAFETY: The channel retains ownership of this socket.
        let _ = unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) };
    }

    #[cfg(test)]
    pub(super) fn without_pidfd(mut self) -> io::Result<Self> {
        if self.pass_pidfd {
            set_socket_option(self.socket.as_raw_fd(), libc::SO_PASSPIDFD, 0)?;
            self.pass_pidfd = false;
        }
        Ok(self)
    }

    /// The caller supplies a fresh random nonce for every observation and owns
    /// its deadline. The channel supplies the bound preparation ID.
    /// Success binds the packet to the request but does not verify its sender's
    /// machine membership or the reported guest-to-host ID mappings.
    pub async fn observe(
        &mut self,
        nonce: [u8; IDENTITY_CHALLENGE_BYTES],
        request: &PrepareRequest<'_>,
    ) -> io::Result<(GuestIdentityReply, IdentityPeerCredentials)> {
        if self.poisoned {
            return Err(invalid_data("identity channel cannot be reused"));
        }
        let challenge = self.ready.identity_challenge(nonce);
        let mut challenge_packet = [0; CHALLENGE_BUFFER_BYTES];
        let length = challenge
            .encode_packet(&mut challenge_packet)
            .map_err(invalid_identity)?;
        let mut exchange = IdentityExchange {
            socket: &self.socket,
            poisoned: &mut self.poisoned,
            completed: false,
        };
        send_packet(exchange.socket, &challenge_packet[..length]).await?;
        let (packet, credentials) = receive_packet(exchange.socket, self.pass_pidfd).await?;
        let reply = GuestIdentityReply::decode_for(&packet, challenge, request)
            .map_err(invalid_identity)?;
        exchange.completed = true;
        Ok((reply, credentials))
    }
}

struct IdentityExchange<'a> {
    socket: &'a AsyncFd<OwnedFd>,
    poisoned: &'a mut bool,
    completed: bool,
}

impl Drop for IdentityExchange<'_> {
    fn drop(&mut self) {
        if !self.completed {
            *self.poisoned = true;
            // SAFETY: The AsyncFd owns this live socket throughout the exchange.
            let _ = unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) };
        }
    }
}

fn socket_option(fd: RawFd, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value = 0;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: The value and length buffers match the requested integer option.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn verify_socket(fd: RawFd) -> io::Result<()> {
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_SEQPACKET
        || socket_option(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity channel requires AF_UNIX/SOCK_SEQPACKET",
        ));
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl only reads or updates flags on the owned socket.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn enable_passpidfd(fd: RawFd) -> io::Result<bool> {
    match set_socket_option(fd, libc::SO_PASSPIDFD, 1) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOPROTOOPT | libc::EOPNOTSUPP | libc::EINVAL)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn set_socket_option(fd: RawFd, option: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: These socket options read one integer; the socket remains owned.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&value as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as _,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn send_packet(socket: &AsyncFd<OwnedFd>, packet: &[u8]) -> io::Result<()> {
    loop {
        let mut ready = socket.writable().await?;
        match ready.try_io(|fd| send_packet_once(fd.as_raw_fd(), packet)) {
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(result) => return result,
            Err(_would_block) => continue,
        }
    }
}

fn send_packet_once(fd: RawFd, packet: &[u8]) -> io::Result<()> {
    // SAFETY: send borrows the packet for this call and never writes to it.
    let sent = unsafe {
        libc::send(
            fd,
            packet.as_ptr().cast(),
            packet.len(),
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "identity packet was only partially sent",
        ));
    }
    Ok(())
}

async fn receive_packet(
    socket: &AsyncFd<OwnedFd>,
    pass_pidfd: bool,
) -> io::Result<(Vec<u8>, IdentityPeerCredentials)> {
    let mut packet = vec![0; MAX_IDENTITY_MESSAGE_BYTES];
    loop {
        let mut ready = socket.readable().await?;
        match ready.try_io(|fd| receive_packet_once(fd.as_raw_fd(), &mut packet, pass_pidfd)) {
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(result) => {
                let (length, credentials) = result?;
                packet.truncate(length);
                return Ok((packet, credentials));
            }
            Err(_would_block) => continue,
        }
    }
}

fn receive_packet_once(
    fd: RawFd,
    packet: &mut [u8],
    pass_pidfd: bool,
) -> io::Result<(usize, IdentityPeerCredentials)> {
    let mut vector = libc::iovec {
        iov_base: packet.as_mut_ptr().cast(),
        iov_len: packet.len(),
    };
    let mut control = AncillaryBuffer([0; 16]);
    // SAFETY: Null name and unused fields describe an unnamed socket message.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen = size_of::<AncillaryBuffer>() as _;
    // SAFETY: Both receive buffers remain live and their lengths bound kernel writes.
    let received = unsafe {
        libc::recvmsg(
            fd,
            &mut message,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
        )
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    // Truncation can still deliver SCM_RIGHTS. Parse and close them before
    // reporting any packet or credentials error.
    let credentials = parse_credentials(&message, &control, pass_pidfd);
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(invalid_data(
            "identity packet or credentials were truncated",
        ));
    }
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "identity channel closed or received an empty packet",
        ));
    }
    Ok((received as usize, credentials?))
}

fn parse_credentials(
    message: &libc::msghdr,
    control: &AncillaryBuffer,
    pass_pidfd: bool,
) -> io::Result<IdentityPeerCredentials> {
    let control_bytes: usize = message.msg_controllen as _;
    let length = control_bytes.min(size_of::<AncillaryBuffer>());
    // CMSG_LEN(0) includes the native ABI padding before the record payload.
    let header_bytes = unsafe { libc::CMSG_LEN(0) as usize };
    let alignment = size_of::<usize>();
    let mut offset = 0;
    let mut credentials = None;
    let mut pidfd = None;
    let mut pidfd_seen = false;
    let mut unexpected = false;
    while length.saturating_sub(offset) >= header_bytes {
        // SAFETY: This complete header lies in the returned control range.
        let header = unsafe {
            control
                .0
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<libc::cmsghdr>()
                .read_unaligned()
        };
        let record_bytes: usize = header.cmsg_len as _;
        let declared_bytes = record_bytes
            .checked_sub(header_bytes)
            .ok_or_else(|| invalid_data("malformed identity ancillary header"))?;
        let available_bytes = length - offset - header_bytes;
        let truncated = declared_bytes > available_bytes;
        let data_bytes = declared_bytes.min(available_bytes);
        // SAFETY: The complete header fits; payload reads are bounded below.
        let data = unsafe { control.0.as_ptr().cast::<u8>().add(offset + header_bytes) };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_CREDENTIALS {
            if truncated || credentials.is_some() || data_bytes != size_of::<libc::ucred>() {
                unexpected = true;
            } else {
                // SAFETY: SCM_CREDENTIALS has exactly one complete ucred above.
                let value = unsafe { data.cast::<libc::ucred>().read_unaligned() };
                credentials = Some(IdentityPeerCredentials {
                    pid: value.pid,
                    uid: value.uid,
                    gid: value.gid,
                    pidfd: None,
                });
            }
        } else if header.cmsg_level == libc::SOL_SOCKET
            && matches!(header.cmsg_type, libc::SCM_RIGHTS | SCM_PIDFD)
        {
            let sender_pidfd = header.cmsg_type == SCM_PIDFD;
            let valid_pidfd = !truncated
                && sender_pidfd
                && !pidfd_seen
                && data_bytes == size_of::<RawFd>()
                && pass_pidfd;
            pidfd_seen |= sender_pidfd;
            unexpected |= !valid_pidfd;
            for index in 0..data_bytes / size_of::<RawFd>() {
                // SAFETY: SCM_RIGHTS and SCM_PIDFD transfer ownership of
                // their nonnegative descriptors to the receiver.
                let fd = unsafe {
                    data.add(index * size_of::<RawFd>())
                        .cast::<RawFd>()
                        .read_unaligned()
                };
                if fd < 0 {
                    // SCM_PIDFD can contain a negative errno if the kernel
                    // could not obtain a handle for the sender.
                    unexpected = true;
                    continue;
                }
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                if valid_pidfd {
                    pidfd = Some(fd);
                }
            }
        } else {
            unexpected = true;
        }
        if truncated {
            return Err(invalid_data(
                "identity ancillary record exceeded its buffer",
            ));
        }
        offset += (record_bytes + alignment - 1) & !(alignment - 1);
    }
    if control_bytes > size_of::<AncillaryBuffer>() {
        return Err(invalid_data("identity ancillary data exceeded its buffer"));
    }
    if unexpected {
        return Err(invalid_data(
            "identity channel received unexpected ancillary data",
        ));
    }
    if pass_pidfd && pidfd.is_none() {
        return Err(invalid_data("identity reply had no sender pidfd"));
    }
    match credentials {
        Some(mut value) if value.pid > 0 => {
            value.pidfd = pidfd;
            Ok(value)
        }
        Some(_) => Err(invalid_data(
            "identity sender PID is not visible to the host",
        )),
        None => Err(invalid_data("identity reply had no SCM_CREDENTIALS")),
    }
}

fn invalid_identity(error: IdentityChannelError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid guest identity packet: {error:?}"),
    )
}

pub(super) fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
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
            message.msg_controllen =
                (offset + fd_record(&mut control, offset, SCM_PIDFD, &fds)) as _;
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
}

#[cfg(test)]
#[path = "../../tests/identity/ancillary.rs"]
mod ancillary_tests;
