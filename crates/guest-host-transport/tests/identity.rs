use lasper_guest_host_transport::UnixIdentityChannel;
use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestIdentityReply, GuestProcess, IdentityChallenge,
    PreparationId, PrepareReady, PrepareReadyFrame, PrepareRequest, PrepareRequestFrame,
    ProjectionRequests, MAX_IDENTITY_MESSAGE_BYTES,
};
use lasper_guest_runtime::identity::{GuestIdentityChannel, IdentityTransportError};
use std::io::{self, Read};
use std::mem::zeroed;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
    let mut sockets = [-1; 2];
    // SAFETY: socketpair writes exactly two new descriptors into this array.
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    };
    assert_eq!(result, 0, "{}", io::Error::last_os_error());
    // SAFETY: Each returned descriptor has exactly one owner.
    unsafe {
        (
            OwnedFd::from_raw_fd(sockets[0]),
            OwnedFd::from_raw_fd(sockets[1]),
        )
    }
}

fn request() -> PrepareRequest<'static> {
    PrepareRequest::new(
        "alice",
        ExecutionKind::Bounded,
        GuestProcess::Command(CommandArgv::new(&["/usr/bin/id"]).unwrap()),
        None,
        None,
        ProjectionRequests::default(),
    )
    .unwrap()
}

fn challenge(nonce: u8) -> IdentityChallenge {
    challenge_for(PreparationId::new([0x31; 16]), nonce)
}

fn challenge_for(preparation_id: PreparationId, nonce: u8) -> IdentityChallenge {
    ready_for(preparation_id).identity_challenge([nonce; 32])
}

fn ready() -> PrepareReadyFrame {
    ready_for(PreparationId::new([0x31; 16]))
}

fn ready_for(preparation_id: PreparationId) -> PrepareReadyFrame {
    let broker = BrokerId::new([0x12; 16]);
    PrepareRequestFrame::new(1, broker, request())
        .ready_frame(PrepareReady::new(preparation_id, broker, 30_000).unwrap())
        .unwrap()
}

fn reply(challenge: IdentityChallenge) -> GuestIdentityReply {
    GuestIdentityReply::new(
        challenge,
        &request(),
        "alice",
        1234,
        5678,
        Default::default(),
    )
    .unwrap()
}

fn guest(socket: OwnedFd) -> GuestIdentityChannel {
    // SAFETY: The socketpair created a connected blocking seqpacket endpoint;
    // ownership moves into the channel and is not retained elsewhere.
    unsafe {
        GuestIdentityChannel::from_raw_fd(socket.into_raw_fd(), PreparationId::new([0x31; 16]))
    }
}

fn send_packet(fd: RawFd, bytes: &[u8], descriptors: &[RawFd]) {
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 256];
    // SAFETY: The used fields are filled with pointers to the buffers below.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    if !descriptors.is_empty() {
        let count = std::mem::size_of_val(descriptors);
        let space = unsafe { libc::CMSG_SPACE(count as _) as usize };
        assert!(space <= size_of_val(&control));
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        // SAFETY: The reserved buffer holds a header and all descriptor slots.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(count as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                count,
            );
        }
    }
    // SAFETY: sendmsg only borrows the live payload/control buffers.
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    assert_eq!(sent, bytes.len() as isize, "{}", io::Error::last_os_error());
}

fn receive_challenge(fd: RawFd) -> IdentityChallenge {
    let mut packet = [0; 256];
    // SAFETY: The kernel writes at most packet.len() bytes into this live array.
    let length = unsafe { libc::recv(fd, packet.as_mut_ptr().cast(), packet.len(), 0) };
    assert!(length > 0, "{}", io::Error::last_os_error());
    IdentityChallenge::decode_packet(&packet[..length as usize]).unwrap()
}

fn assert_peer_closed(mut peer: UnixStream) {
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut byte = [0];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}

fn enable_sender_pidfd(socket: RawFd) -> bool {
    let enabled: libc::c_int = 1;
    // SAFETY: SO_PASSPIDFD reads this one live integer.
    let result = unsafe {
        libc::setsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_PASSPIDFD,
            (&enabled as *const libc::c_int).cast(),
            size_of_val(&enabled) as _,
        )
    };
    if result == 0 {
        true
    } else {
        let error = io::Error::last_os_error();
        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ENOPROTOOPT | libc::EOPNOTSUPP | libc::EINVAL)
            ),
            "{error}"
        );
        false
    }
}

fn assert_pidfd_matches(credentials: &lasper_guest_host_transport::IdentityPeerCredentials) {
    if let Some(pidfd) = credentials.pidfd() {
        let information =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).unwrap();
        let sender = format!("Pid:\t{}", credentials.pid());
        assert!(information.lines().any(|line| line == sender));
        // SAFETY: The credentials object still owns this borrowed descriptor.
        let flags = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
}

#[tokio::test]
async fn host_configures_only_its_endpoint_for_nonblocking_io() {
    let (host, worker) = seqpacket_pair();
    let host_fd = host.as_raw_fd();
    let _host = UnixIdentityChannel::new(host, ready()).unwrap();
    // SAFETY: Both descriptors remain owned and live for these flag reads.
    unsafe {
        assert_ne!(libc::fcntl(host_fd, libc::F_GETFL) & libc::O_NONBLOCK, 0);
        assert_eq!(
            libc::fcntl(worker.as_raw_fd(), libc::F_GETFL) & libc::O_NONBLOCK,
            0
        );
    }
}

#[tokio::test]
async fn identity_exchange_uses_kernel_credentials_and_can_recheck() {
    let (host, worker) = seqpacket_pair();
    let pidfd_supported = enable_sender_pidfd(host.as_raw_fd());
    let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
    let task = tokio::task::spawn_blocking(move || {
        let mut worker = guest(worker);
        let mut packet = [0; 2048];
        for nonce in [0x72, 0x73] {
            let actual = worker.receive_challenge(&mut packet).unwrap();
            assert_eq!(actual, challenge(nonce));
            worker.send_reply(&reply(actual), &mut packet).unwrap();
        }
    });
    for nonce in [0x72, 0x73] {
        let (identity, credentials) = host.observe([nonce; 32], &request()).await.unwrap();
        assert_eq!(identity.guest_uid(), 1234);
        assert_eq!(identity.guest_gid(), 5678);
        assert_eq!(credentials.pid(), std::process::id() as i32);
        // SAFETY: These calls only read the calling thread's credentials.
        assert_eq!(credentials.uid(), unsafe { libc::getuid() });
        assert_eq!(credentials.gid(), unsafe { libc::getgid() });
        assert_eq!(credentials.pidfd().is_some(), pidfd_supported);
        assert_pidfd_matches(&credentials);
        assert!(host.can_continue());
    }
    task.await.unwrap();
}

#[test]
fn guest_rejects_a_challenge_for_another_preparation() {
    let (host, worker) = seqpacket_pair();
    let mut worker = guest(worker);
    let foreign = challenge_for(PreparationId::new([0x32; 16]), 0x72);
    let mut packet = [0; 2048];
    let length = foreign.encode_packet(&mut packet).unwrap();
    send_packet(host.as_raw_fd(), &packet[..length], &[]);
    assert_eq!(
        worker.receive_challenge(&mut packet),
        Err(IdentityTransportError::PreparationMismatch)
    );
    assert!(!worker.can_continue());
    assert_eq!(
        worker.send_reply(&reply(foreign), &mut packet),
        Err(IdentityTransportError::Poisoned)
    );
}

#[test]
fn guest_rejects_a_kernel_generated_pidfd() {
    let (host, worker) = seqpacket_pair();
    if !enable_sender_pidfd(worker.as_raw_fd()) {
        return;
    }
    let mut worker = guest(worker);
    let mut packet = [0; 2048];
    let length = challenge(0x72).encode_packet(&mut packet).unwrap();
    send_packet(host.as_raw_fd(), &packet[..length], &[]);
    assert_eq!(
        worker.receive_challenge(&mut packet),
        Err(IdentityTransportError::UnexpectedAncillary)
    );
    assert!(!worker.can_continue());
}

#[tokio::test]
async fn host_rejects_stream_socket() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    assert_eq!(
        UnixIdentityChannel::new(stream.into(), ready())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn host_rejects_replies_from_another_check_or_preparation() {
    for stale in [
        challenge(0x71),
        challenge_for(PreparationId::new([0x32; 16]), 0x72),
    ] {
        let (host, worker) = seqpacket_pair();
        let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
        let task = tokio::task::spawn_blocking(move || {
            assert_eq!(receive_challenge(worker.as_raw_fd()), challenge(0x72));
            let mut packet = [0; 2048];
            let length = reply(stale).encode_packet(&mut packet).unwrap();
            send_packet(worker.as_raw_fd(), &packet[..length], &[]);
        });
        let error = host.observe([0x72; 32], &request()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!host.can_continue());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn host_cancellation_invalidates_the_channel() {
    let (host, worker) = seqpacket_pair();
    let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
    let error = tokio::time::timeout(
        Duration::from_millis(20),
        host.observe([0x72; 32], &request()),
    )
    .await;
    assert!(error.is_err());
    assert!(!host.can_continue());
    assert_eq!(
        host.observe([0x73; 32], &request())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    drop(worker);
}

#[tokio::test]
async fn host_rejects_eof_without_waiting_for_credentials() {
    let (host, worker) = seqpacket_pair();
    let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
    let task = tokio::task::spawn_blocking(move || {
        let _ = receive_challenge(worker.as_raw_fd());
    });
    assert_eq!(
        host.observe([0x72; 32], &request())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(!host.can_continue());
    task.await.unwrap();
}

#[tokio::test]
async fn host_rejects_rights_and_closes_delivered_fds_even_on_truncation() {
    for (bytes, descriptor_count) in [(1, 1), (MAX_IDENTITY_MESSAGE_BYTES + 1, 1), (1, 64)] {
        let (host, worker) = seqpacket_pair();
        let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
        let (transferred, peer) = UnixStream::pair().unwrap();
        let task = tokio::task::spawn_blocking(move || {
            let _ = receive_challenge(worker.as_raw_fd());
            send_packet(
                worker.as_raw_fd(),
                &vec![0; bytes],
                &vec![transferred.as_raw_fd(); descriptor_count],
            );
        });
        let error = host.observe([0x72; 32], &request()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!host.can_continue());
        task.await.unwrap();
        assert_peer_closed(peer);
    }
}

#[test]
fn guest_retains_pending_challenge_after_pre_io_errors() {
    let (host, worker) = seqpacket_pair();
    let mut worker = guest(worker);
    let mut packet = [0; 2048];
    assert_eq!(
        worker.send_reply(&reply(challenge(0x72)), &mut packet),
        Err(IdentityTransportError::UnexpectedPhase)
    );
    let current = challenge(0x72);
    let length = current.encode_packet(&mut packet).unwrap();
    send_packet(host.as_raw_fd(), &packet[..length], &[]);
    assert_eq!(worker.receive_challenge(&mut packet).unwrap(), current);
    assert_eq!(
        worker.receive_challenge(&mut packet),
        Err(IdentityTransportError::UnexpectedPhase)
    );
    assert_eq!(
        worker.send_reply(&reply(challenge(0x73)), &mut packet),
        Err(IdentityTransportError::ChallengeMismatch)
    );
    assert!(worker.send_reply(&reply(current), &mut [0; 1]).is_err());
    assert!(worker.can_continue());
    worker.send_reply(&reply(current), &mut packet).unwrap();
    // Reading as a challenge would reject this reply: consume the raw packet.
    let mut reply_packet = [0; 2048];
    let length = unsafe {
        libc::recv(
            host.as_raw_fd(),
            reply_packet.as_mut_ptr().cast(),
            reply_packet.len(),
            0,
        )
    };
    assert!(length > 0);
    assert!(
        GuestIdentityReply::decode_for(&reply_packet[..length as usize], current, &request())
            .is_ok()
    );
}

#[test]
fn guest_rejects_rights_and_closes_delivered_fds_even_on_truncation() {
    for (bytes, descriptor_count) in [(1, 1), (2048, 1), (1, 64)] {
        let (host, worker) = seqpacket_pair();
        let mut worker = guest(worker);
        let (transferred, peer) = UnixStream::pair().unwrap();
        send_packet(
            host.as_raw_fd(),
            &vec![0; bytes],
            &vec![transferred.as_raw_fd(); descriptor_count],
        );
        drop(transferred);
        assert!(worker.receive_challenge(&mut [0; 512]).is_err());
        assert!(!worker.can_continue());
        assert_peer_closed(peer);
    }
}

#[test]
fn guest_rejects_eof_and_empty_packet() {
    for empty_packet in [false, true] {
        let (host, worker) = seqpacket_pair();
        let mut worker = guest(worker);
        if empty_packet {
            send_packet(host.as_raw_fd(), &[], &[]);
        } else {
            drop(host);
        }
        assert_eq!(
            worker.receive_challenge(&mut [0; 256]),
            Err(IdentityTransportError::UnexpectedEof)
        );
        assert!(!worker.can_continue());
    }
}

struct ForkedSender(libc::pid_t);

impl ForkedSender {
    fn wait(mut self) -> libc::c_int {
        let mut status = 0;
        // SAFETY: This test owns the unreaped child; no other code waits on it.
        let waited = unsafe { libc::waitpid(self.0, &mut status, 0) };
        assert_eq!(waited, self.0);
        self.0 = 0;
        status
    }
}

impl Drop for ForkedSender {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: The unreaped child keeps its PID reserved. On a failed
            // assertion, stop it before waiting so the test cannot leave it behind.
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                libc::waitpid(self.0, std::ptr::null_mut(), 0);
            }
        }
    }
}

#[tokio::test]
async fn credentials_identify_the_sender_after_fork_not_the_socket_creator() {
    let (host_fd, worker_fd) = seqpacket_pair();
    let mut packet = [0; 2048];
    let current = challenge(0x72);
    let length = reply(current).encode_packet(&mut packet).unwrap();
    let host_raw = host_fd.as_raw_fd();
    let worker_raw = worker_fd.as_raw_fd();
    let mut host = UnixIdentityChannel::new(host_fd, ready()).unwrap();
    // SAFETY: After fork the child uses only async-signal-safe syscalls and
    // _exit; it neither runs Rust destructors nor touches the Tokio runtime.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "{}", io::Error::last_os_error());
    if pid == 0 {
        let mut incoming = [0u8; 256];
        unsafe {
            libc::close(host_raw);
            let read = libc::recv(worker_raw, incoming.as_mut_ptr().cast(), incoming.len(), 0);
            let sent = if read > 0 {
                libc::send(
                    worker_raw,
                    packet.as_ptr().cast(),
                    length,
                    libc::MSG_NOSIGNAL,
                )
            } else {
                -1
            };
            libc::_exit(if sent == length as isize { 0 } else { 1 });
        }
    }
    let child = ForkedSender(pid);
    drop(worker_fd);
    let (_, credentials) = tokio::time::timeout(
        Duration::from_secs(2),
        host.observe(*current.nonce(), &request()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(credentials.pid(), pid);
    assert_ne!(credentials.pid(), std::process::id() as i32);
    assert_pidfd_matches(&credentials);
    let status = child.wait();
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    if let Some(pidfd) = credentials.pidfd() {
        let mut descriptor = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows this one live pidfd and does not block.
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);
        assert_ne!(descriptor.revents & (libc::POLLIN | libc::POLLHUP), 0);
    }
}

#[tokio::test]
async fn a_full_send_queue_does_not_block_the_async_runtime() {
    let (host, worker) = seqpacket_pair();
    let filler = [0u8; 256];
    loop {
        // SAFETY: The payload is live and MSG_DONTWAIT keeps queue filling bounded.
        let sent = unsafe {
            libc::send(
                host.as_raw_fd(),
                filler.as_ptr().cast(),
                filler.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            break;
        }
    }
    let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        host.observe([0x72; 32], &request())
    )
    .await
    .is_err());
    assert!(!host.can_continue());
    drop(worker);
}

#[tokio::test]
async fn host_accepts_one_complete_packet_at_the_identity_limit() {
    let (host, worker) = seqpacket_pair();
    let mut host = UnixIdentityChannel::new(host, ready()).unwrap();
    let current = challenge(0x72);
    let task = tokio::task::spawn_blocking(move || {
        assert_eq!(receive_challenge(worker.as_raw_fd()), current);
        let mut packet = vec![b' '; MAX_IDENTITY_MESSAGE_BYTES];
        reply(current).encode_packet(&mut packet).unwrap();
        send_packet(worker.as_raw_fd(), &packet, &[]);
    });
    assert_eq!(
        host.observe(*current.nonce(), &request()).await.unwrap().0,
        reply(current)
    );
    task.await.unwrap();
}

#[test]
fn guest_caps_receive_length_even_when_caller_supplies_a_larger_buffer() {
    let (host, worker) = seqpacket_pair();
    let mut worker = guest(worker);
    send_packet(
        host.as_raw_fd(),
        &vec![b' '; MAX_IDENTITY_MESSAGE_BYTES + 1],
        &[],
    );
    assert_eq!(
        worker.receive_challenge(&mut vec![0; MAX_IDENTITY_MESSAGE_BYTES + 1024]),
        Err(IdentityTransportError::Truncated)
    );
    assert!(!worker.can_continue());
}
