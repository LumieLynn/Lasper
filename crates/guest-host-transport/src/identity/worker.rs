use super::channel::invalid_data;
use super::process::WorkerProcess;
use super::UnixIdentityChannel;
use lasper_guest_protocol::{
    GuestIdentityReply, PrepareRequest, PrepareRequestFrame, IDENTITY_CHALLENGE_BYTES,
};
use std::io;
use std::sync::Arc;

/// Keeps the identity channel and its request together for Prepare and Recheck.
/// The worker is assigned before the first reply. Reported guest IDs must map
/// to its kernel credentials; machine membership still needs checking before
/// desktop authorization. Account-name resolution belongs to the trusted
/// guest helper, not this credential check.
pub struct IdentityWorker<'payload> {
    channel: UnixIdentityChannel,
    request: Arc<PrepareRequestFrame<'payload>>,
    process: WorkerProcess,
    observed_ids: Option<ObservedIds>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ObservedIds {
    host_uid: u32,
    host_gid: u32,
    guest_uid: u32,
    guest_gid: u32,
}

impl<'payload> IdentityWorker<'payload> {
    pub(crate) fn new(
        channel: UnixIdentityChannel,
        request: Arc<PrepareRequestFrame<'payload>>,
        process: WorkerProcess,
    ) -> Self {
        Self {
            channel,
            request,
            process,
            observed_ids: None,
        }
    }

    pub fn process(&self) -> &WorkerProcess {
        &self.process
    }

    pub fn can_continue(&self) -> bool {
        self.channel.can_continue()
    }

    pub(crate) fn invalidate(&mut self) {
        self.channel.invalidate();
    }

    /// The caller owns the deadline. Cancellation invalidates the channel,
    /// including cancellation between receiving a reply and verifying procfs.
    pub async fn observe(&mut self) -> io::Result<WorkerObservation> {
        if !self.channel.can_continue() {
            return Err(invalid_data("identity worker cannot be reused"));
        }
        let mut check = WorkerCheck {
            channel: &mut self.channel,
            completed: false,
        };
        let result = observe_worker(check.channel, self.request.request(), &self.process)
            .await
            .and_then(|observation| {
                let ids = ObservedIds {
                    host_uid: observation.host_uid,
                    host_gid: observation.host_gid,
                    guest_uid: observation.reply.guest_uid(),
                    guest_gid: observation.reply.guest_gid(),
                };
                if self.observed_ids.is_some_and(|previous| previous != ids) {
                    return Err(invalid_data("worker identity changed during recheck"));
                }
                self.observed_ids = Some(ids);
                Ok(observation)
            });
        if result.is_ok() {
            check.completed = true;
        }
        result
    }
}

/// A guest report whose IDs map to the kernel credentials of its live sender.
/// The process has not yet been matched to a target machine. The account name
/// matches the request, but its relationship to these IDs relies on the guest
/// helper resolving and switching to that account correctly.
#[derive(Debug)]
pub struct WorkerObservation {
    reply: GuestIdentityReply,
    host_uid: u32,
    host_gid: u32,
}

impl WorkerObservation {
    pub fn reply(&self) -> &GuestIdentityReply {
        &self.reply
    }
    pub const fn host_uid(&self) -> u32 {
        self.host_uid
    }
    pub const fn host_gid(&self) -> u32 {
        self.host_gid
    }
}

async fn observe_worker(
    channel: &mut UnixIdentityChannel,
    request: &PrepareRequest<'_>,
    process: &WorkerProcess,
) -> io::Result<WorkerObservation> {
    process.recheck()?;
    let (reply, credentials) = channel.observe(fresh_nonce()?, request).await?;
    process.verify_identity(&credentials, reply.guest_uid(), reply.guest_gid())?;
    Ok(WorkerObservation {
        reply,
        host_uid: credentials.uid(),
        host_gid: credentials.gid(),
    })
}

struct WorkerCheck<'a> {
    channel: &'a mut UnixIdentityChannel,
    completed: bool,
}

impl Drop for WorkerCheck<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.channel.invalidate();
        }
    }
}

fn fresh_nonce() -> io::Result<[u8; IDENTITY_CHALLENGE_BYTES]> {
    let mut nonce = [0; IDENTITY_CHALLENGE_BYTES];
    let mut remaining = nonce.as_mut_slice();
    while !remaining.is_empty() {
        // SAFETY: getrandom writes only into the remaining part of this array.
        let read = unsafe {
            libc::getrandom(
                remaining.as_mut_ptr().cast(),
                remaining.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned no data",
            ));
        }
        remaining = &mut remaining[read as usize..];
    }
    Ok(nonce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lasper_guest_protocol::{
        BrokerId, CommandArgv, ExecutionKind, GuestProcess, PreparationId, PrepareReady,
        PrepareRequestFrame, ProjectionRequests, MAX_IDENTITY_MESSAGE_BYTES,
    };
    use lasper_guest_runtime::identity::GuestIdentityChannel;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::time::Duration;

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

    fn channels(old_kernel: bool) -> (UnixIdentityChannel, GuestIdentityChannel, i32, i32) {
        let mut sockets = [-1; 2];
        // SAFETY: socketpair initializes the two descriptor slots on success.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: Each descriptor has exactly one owner, transferred below.
        let (host, guest) = unsafe {
            (
                OwnedFd::from_raw_fd(sockets[0]),
                OwnedFd::from_raw_fd(sockets[1]),
            )
        };
        let broker = BrokerId::new([1; 16]);
        let preparation = PreparationId::new([2; 16]);
        let ready = PrepareRequestFrame::new(7, broker, request())
            .ready_frame(PrepareReady::new(preparation, broker, 30_000).unwrap())
            .unwrap();
        let host_fd = host.as_raw_fd();
        let host = UnixIdentityChannel::new(host, ready).unwrap();
        let host = if old_kernel {
            host.without_pidfd().unwrap()
        } else {
            host
        };
        // SAFETY: This transfers exclusive ownership of a blocking seqpacket
        // endpoint. Its preparation is assigned before accepting any challenge.
        let guest_fd = guest.as_raw_fd();
        let guest = unsafe { GuestIdentityChannel::from_raw_fd(guest.into_raw_fd(), preparation) };
        (host, guest, guest_fd, host_fd)
    }

    fn owner<'payload>(
        channel: UnixIdentityChannel,
        request: &PrepareRequest<'payload>,
        pid: libc::pid_t,
    ) -> IdentityWorker<'payload> {
        // SAFETY: These tests retain their own process or an unreaped child,
        // so obtaining the assigned pidfd cannot race with PID reuse.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        // SAFETY: pidfd_open returned a new descriptor owned by this value.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let request = Arc::new(PrepareRequestFrame::new(
            7,
            BrokerId::new([1; 16]),
            request.clone(),
        ));
        IdentityWorker::new(channel, request, WorkerProcess::open(pidfd).unwrap())
    }

    struct Child(libc::pid_t);

    impl Child {
        fn wait(&mut self) {
            let mut status = 0;
            // SAFETY: This is the unreaped child owned by this test.
            loop {
                let waited = unsafe { libc::waitpid(self.0, &mut status, 0) };
                if waited < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                assert_eq!(waited, self.0);
                self.0 = 0;
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                break;
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            if self.0 > 0 {
                // SAFETY: The unreaped child reserves this PID. Failed tests
                // must still stop it and release its process table entry.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    while libc::waitpid(self.0, std::ptr::null_mut(), 0) < 0 {
                        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                            break;
                        }
                    }
                }
            }
        }
    }

    fn fork_reply(guest: &mut GuestIdentityChannel) -> bool {
        let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
        let Ok(challenge) = guest.receive_challenge(&mut bytes) else {
            return false;
        };
        let Ok(response) = GuestIdentityReply::new(
            challenge,
            &request(),
            "alice",
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
            Default::default(),
        ) else {
            return false;
        };
        guest.send_reply(&response, &mut bytes).is_ok()
    }

    #[tokio::test]
    async fn an_exited_worker_cannot_be_rechecked_through_its_pinned_proc_handle() {
        for old_kernel in [false, true] {
            let (host, mut guest, _, host_fd) = channels(old_kernel);
            let mut gate = [-1; 2];
            // SAFETY: pipe2 initializes two owned descriptors on success.
            assert_eq!(
                unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(gate[0]), OwnedFd::from_raw_fd(gate[1])) };
            // SAFETY: The child uses only raw syscalls and the allocation-free
            // guest codec. It exits without running inherited destructors.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::close(host_fd);
                    libc::close(write.as_raw_fd());
                }
                if !fork_reply(&mut guest) {
                    unsafe { libc::_exit(1) };
                }
                let mut byte = 0u8;
                unsafe {
                    libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(0);
                }
            }
            let mut child = Child(pid);
            drop(read);
            drop(guest);
            let request = request();
            let mut owner = owner(host, &request, pid);
            tokio::time::timeout(Duration::from_secs(2), owner.observe())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(owner.process().pid(), pid);
            File::from(write).write_all(&[1]).unwrap();
            child.wait();
            assert!(owner.process().recheck().is_err());
            assert!(owner.observe().await.is_err());
            assert!(!owner.can_continue());
        }
    }

    #[tokio::test]
    async fn another_process_cannot_replace_the_assigned_worker_on_recheck() {
        use std::os::unix::net::UnixStream;
        let (host, mut guest, _, host_fd) = channels(true);
        let (mut parent, child_gate) = UnixStream::pair().unwrap();
        // SAFETY: The child only uses the allocation-free guest codec and raw
        // syscalls, and exits without touching the inherited Tokio runtime.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(host_fd);
                libc::close(parent.as_raw_fd());
            }
            if !fork_reply(&mut guest) {
                unsafe { libc::_exit(1) };
            }
            let byte = 1u8;
            unsafe {
                libc::write(child_gate.as_raw_fd(), (&byte as *const u8).cast(), 1);
            }
            let mut byte = 0u8;
            unsafe {
                libc::read(child_gate.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        let mut child = Child(pid);
        drop(child_gate);
        let request = request();
        let mut owner = owner(host, &request, pid);
        owner.observe().await.unwrap();
        parent.read_exact(&mut [0; 1]).unwrap();
        let replacement =
            tokio::task::spawn_blocking(move || reply(&mut guest, unsafe { libc::getuid() }));
        let error = tokio::time::timeout(Duration::from_secs(2), owner.observe())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("different worker"));
        assert!(!owner.can_continue());
        replacement.await.unwrap();
        parent.write_all(&[1]).unwrap();
        drop(parent);
        child.wait();
    }

    #[tokio::test]
    async fn first_reply_cannot_claim_a_different_assigned_process() {
        let (host, mut guest, guest_fd, host_fd) = channels(true);
        let (mut parent, child_gate) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: The child only waits on a private descriptor and exits. It
        // does not use inherited runtime locks or run parent destructors.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(host_fd);
                libc::close(parent.as_raw_fd());
                libc::close(guest_fd);
                let mut byte = 0u8;
                libc::read(child_gate.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        let mut child = Child(pid);
        drop(child_gate);
        let request = request();
        let mut owner = owner(host, &request, pid);
        let replacement =
            tokio::task::spawn_blocking(move || reply(&mut guest, unsafe { libc::getuid() }));
        let error = owner.observe().await.unwrap_err();
        assert!(error.to_string().contains("different worker"));
        assert!(!owner.can_continue());
        replacement.await.unwrap();
        parent.write_all(&[1]).unwrap();
        child.wait();
    }

    #[tokio::test]
    async fn exited_assignment_is_not_replaced_by_the_live_channel_sender() {
        let (host, _guest, _, host_fd) = channels(true);
        // SAFETY: The child closes an inherited endpoint and immediately exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(host_fd);
                libc::_exit(0);
            }
        }
        let mut child = Child(pid);
        // The child stays unreaped until its assigned pidfd has been acquired.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        child.wait();
        assert!(WorkerProcess::open(pidfd).is_err());
        drop(host);
    }

    fn reply(guest: &mut GuestIdentityChannel, uid: u32) -> [u8; 32] {
        reply_with_ids(guest, uid, unsafe { libc::getgid() })
    }

    fn reply_with_ids(guest: &mut GuestIdentityChannel, uid: u32, gid: u32) -> [u8; 32] {
        let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
        let challenge = guest.receive_challenge(&mut bytes).unwrap();
        let response =
            GuestIdentityReply::new(challenge, &request(), "alice", uid, gid, Default::default())
                .unwrap();
        guest.send_reply(&response, &mut bytes).unwrap();
        *challenge.nonce()
    }

    #[tokio::test]
    async fn first_reply_cannot_substitute_another_uid_or_gid() {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        for (reported_uid, reported_gid, field) in
            [(uid ^ 1, gid, "guest UID"), (uid, gid ^ 1, "guest GID")]
        {
            let (host, mut guest, _, _) = channels(true);
            let task = tokio::task::spawn_blocking(move || {
                reply_with_ids(&mut guest, reported_uid, reported_gid);
            });
            let request = request();
            let mut owner = owner(host, &request, std::process::id() as i32);
            let error = owner.observe().await.unwrap_err();
            assert!(error.to_string().contains(field), "{error}");
            assert!(!owner.can_continue());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn assigned_pidfd_checks_the_sender_without_passpidfd() {
        let (host, mut guest, _, _) = channels(true);
        let request = request();
        let worker = tokio::task::spawn_blocking(move || {
            let nonces = [
                reply(&mut guest, unsafe { libc::getuid() }),
                reply(&mut guest, unsafe { libc::getuid() }),
            ];
            assert_ne!(nonces[0], nonces[1]);
            nonces
        });
        let mut owner = owner(host, &request, std::process::id() as i32);
        let first = tokio::time::timeout(Duration::from_secs(2), owner.observe())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.process().pid(), std::process::id() as i32);
        assert_eq!(first.host_uid(), unsafe { libc::getuid() });
        assert_eq!(first.host_gid(), unsafe { libc::getgid() });
        assert_eq!(first.reply().guest_uid(), unsafe { libc::getuid() });
        let fd = owner.process().directory().as_raw_fd();
        let second = tokio::time::timeout(Duration::from_secs(2), owner.observe())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.process().directory().as_raw_fd(), fd);
        assert_ne!(first.reply().challenge(), second.reply().challenge());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn a_reported_uid_that_does_not_map_invalidates_the_owner() {
        let (host, mut guest, _, _) = channels(true);
        let worker = tokio::task::spawn_blocking(move || {
            reply(&mut guest, unsafe { libc::getuid() });
            reply(&mut guest, unsafe { libc::getuid() } ^ 1);
        });
        let request = request();
        let mut owner = owner(host, &request, std::process::id() as i32);
        owner.observe().await.unwrap();
        let error = owner.observe().await.unwrap_err();
        assert!(error.to_string().contains("guest UID does not map"));
        assert!(!owner.can_continue());
        assert!(owner.observe().await.is_err());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_a_reply_closes_the_channel() {
        let (host, mut guest, guest_fd, _) = channels(true);
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let mut bytes = [0; 256];
            guest.receive_challenge(&mut bytes).unwrap();
            ready.send(()).unwrap();
            // SAFETY: guest still owns this endpoint. A raw read observes EOF
            // without trying to accept another protocol challenge.
            assert_eq!(
                unsafe { libc::recv(guest_fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) },
                0
            );
        });
        let request = request();
        let mut owner = owner(host, &request, std::process::id() as i32);
        let mut observation = Box::pin(owner.observe());
        tokio::select! {
            result = &mut observation => panic!("observation ended before cancellation: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), waiting) => result.unwrap().unwrap(),
        }
        drop(observation);
        assert!(!owner.can_continue());
        worker.await.unwrap();
    }
    #[tokio::test]
    #[ignore = "requires unprivileged user namespaces; CI runs this explicitly"]
    async fn child_user_namespace_maps_packet_and_proc_credentials() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::net::UnixStream;

        let host_uid = unsafe { libc::getuid() };
        let host_gid = unsafe { libc::getgid() };
        let guest_uid = host_uid ^ 1;
        let guest_gid = host_gid ^ 1;
        for wrong_field in [None, Some("guest UID"), Some("guest GID")] {
            let changed_root = File::open("/tmp").unwrap();
            let (host, mut guest, _, host_fd) = channels(true);
            let (mut parent, child_gate) = UnixStream::pair().unwrap();
            parent
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            parent
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            // SAFETY: The child uses raw syscalls and the allocation-free guest
            // codec, then _exit; it never resumes the inherited Tokio runtime.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::close(host_fd);
                    libc::close(parent.as_raw_fd());
                    let error: i32 = if libc::unshare(libc::CLONE_NEWUSER) == 0 {
                        0
                    } else {
                        *libc::__errno_location()
                    };
                    if libc::write(child_gate.as_raw_fd(), (&error as *const i32).cast(), 4) != 4
                        || error != 0
                    {
                        libc::_exit(1);
                    }
                    let mut gate = 0u8;
                    if libc::read(child_gate.as_raw_fd(), (&mut gate as *mut u8).cast(), 1) != 1
                        || libc::getuid() != guest_uid
                        || libc::getgid() != guest_gid
                    {
                        libc::_exit(2);
                    }
                    let mut bytes = [0; MAX_IDENTITY_MESSAGE_BYTES];
                    let Ok(challenge) = guest.receive_challenge(&mut bytes) else {
                        libc::_exit(3);
                    };
                    let uid = if wrong_field == Some("guest UID") {
                        guest_uid ^ 1
                    } else {
                        guest_uid
                    };
                    let gid = if wrong_field == Some("guest GID") {
                        guest_gid ^ 1
                    } else {
                        guest_gid
                    };
                    let Ok(reply) = GuestIdentityReply::new(
                        challenge,
                        &request(),
                        "alice",
                        uid,
                        gid,
                        Default::default(),
                    ) else {
                        libc::_exit(4);
                    };
                    if guest.send_reply(&reply, &mut bytes).is_err() {
                        libc::_exit(5);
                    }
                    // Keep the assigned process alive until host verification
                    // finishes, including on the rejection paths.
                    if libc::read(child_gate.as_raw_fd(), (&mut gate as *mut u8).cast(), 1) != 1 {
                        libc::_exit(6);
                    }
                    if gate == 2 {
                        if libc::fchdir(changed_root.as_raw_fd()) < 0
                            || libc::chroot(c".".as_ptr()) < 0
                        {
                            libc::_exit(7);
                        }
                        if libc::write(child_gate.as_raw_fd(), [1u8].as_ptr().cast(), 1) != 1
                            || libc::read(child_gate.as_raw_fd(), (&mut gate as *mut u8).cast(), 1)
                                != 1
                        {
                            libc::_exit(8);
                        }
                    }
                    libc::_exit(0);
                }
            }
            let mut child = Child(pid);
            drop(child_gate);
            drop(guest);
            let mut error = [0; 4];
            parent.read_exact(&mut error).unwrap();
            let error = i32::from_ne_bytes(error);
            assert_eq!(
                error,
                0,
                "create user namespace: {}",
                io::Error::from_raw_os_error(error)
            );
            let proc = format!("/proc/{pid}");
            std::fs::write(format!("{proc}/setgroups"), "deny").unwrap();
            std::fs::write(
                format!("{proc}/uid_map"),
                format!("{guest_uid} {host_uid} 1\n"),
            )
            .unwrap();
            std::fs::write(
                format!("{proc}/gid_map"),
                format!("{guest_gid} {host_gid} 1\n"),
            )
            .unwrap();
            parent.write_all(&[1]).unwrap();

            let reader_ns = std::fs::metadata("/proc/thread-self/ns/user").unwrap();
            let worker_ns = std::fs::metadata(format!("{proc}/ns/user")).unwrap();
            assert_ne!(
                (reader_ns.dev(), reader_ns.ino()),
                (worker_ns.dev(), worker_ns.ino())
            );
            for (file, inside, outside) in [
                ("uid_map", guest_uid, host_uid),
                ("gid_map", guest_gid, host_gid),
            ] {
                let fields: Vec<u32> = std::fs::read_to_string(format!("{proc}/{file}"))
                    .unwrap()
                    .split_ascii_whitespace()
                    .map(|field| field.parse().unwrap())
                    .collect();
                assert_eq!(fields, [inside, outside, 1]);
            }
            let status = std::fs::read_to_string(format!("{proc}/status")).unwrap();
            for (key, id) in [("Uid:", host_uid), ("Gid:", host_gid)] {
                let ids: Vec<u32> = status
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .unwrap()
                    .split_ascii_whitespace()
                    .map(|id| id.parse().unwrap())
                    .collect();
                assert_eq!(ids, [id; 4]);
            }
            let request = request();
            let mut owner = owner(host, &request, pid);
            // Same PID/mount namespace and root, but a different user namespace
            // must not qualify as the reader's target process.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
            assert!(fd >= 0);
            let reader = crate::MachineTarget::from_leader(
                unsafe { OwnedFd::from_raw_fd(fd as i32) },
                std::path::Path::new("/"),
            )
            .unwrap();
            let mismatch = reader.verify_worker(owner.process()).unwrap_err();
            assert!(
                mismatch.to_string().contains("target user namespace"),
                "{mismatch}"
            );
            let result = tokio::time::timeout(Duration::from_secs(5), owner.observe())
                .await
                .unwrap();
            if let Some(field) = wrong_field {
                let error = result.unwrap_err();
                assert!(error.to_string().contains(field), "{error}");
                assert!(!owner.can_continue());
            } else {
                let observed = result.unwrap();
                assert_eq!(
                    (observed.host_uid(), observed.host_gid()),
                    (host_uid, host_gid)
                );
                assert_eq!(
                    (observed.reply().guest_uid(), observed.reply().guest_gid()),
                    (guest_uid, guest_gid)
                );
                parent.write_all(&[2]).unwrap();
                parent.read_exact(&mut [0]).unwrap();
                let error = owner.observe().await.unwrap_err();
                assert!(
                    error.to_string().contains("changed root filesystem"),
                    "{error}"
                );
                assert!(!owner.can_continue());
            }
            parent.write_all(&[1]).unwrap();
            child.wait();
        }
    }
}
