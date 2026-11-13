use crate::control::{drive, receive_guest_frame, send_guest_frame};
use crate::{target, worker};
use lasper_guest_host_transport::{BrokerConnection, BrokerSession};
use lasper_guest_protocol::*;
use lasper_guest_runtime::readiness::GuestWorkerReadiness;
use lasper_guest_runtime::transport::GuestControlStream;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixListener;
use std::task::Poll;
use std::time::Duration;
use tokio::time::Instant;

pub(super) const BROKER: BrokerId = BrokerId::new([0x81; 16]);
pub(super) const PREPARATION: PreparationId = PreparationId::new([0x82; 16]);

pub(super) fn request() -> PrepareRequest<'static> {
    PrepareRequest::new(
        "alice",
        ExecutionKind::Bounded,
        GuestProcess::Command(CommandArgv::new(&["/bin/true"]).unwrap()),
        None,
        None,
        ProjectionRequests::default(),
    )
    .unwrap()
}

pub(super) struct Server {
    directory: tempfile::TempDir,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub(super) fn spawn(
        run: impl FnOnce(GuestControlStream, PrepareRequestFrame<'static>) + Send + 'static,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("broker.sock")).unwrap();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // SAFETY: the accepted endpoint has one owner, transferred here.
            let mut guest = unsafe { GuestControlStream::from_raw_fd(stream.into_raw_fd()) };
            let mut bytes = [0; 4096];
            let hello = receive_guest_frame(&guest, &mut bytes).unwrap();
            let accepted = GuestWorkerReadiness::check()
                .unwrap()
                .accept_hello(
                    hello.header().protocol_version(),
                    HelloRequest::decode_payload(hello.payload()).unwrap(),
                    BROKER,
                    ControlLimits::CURRENT,
                )
                .unwrap();
            let id = hello.header().request_id();
            drop(hello);
            let length = accepted.response().encode_payload(&mut bytes).unwrap();
            send_guest_frame(
                &guest,
                ControlFrameHeader::new(MessageKind::HelloResponse, id, length as u32, 0).unwrap(),
                &bytes[..length],
                &[],
            )
            .unwrap();
            guest.complete_hello(accepted.limits()).unwrap();
            let prepare = receive_guest_frame(&guest, &mut bytes).unwrap();
            let id = {
                let request = PrepareRequestFrame::decode_parts(
                    prepare.header(),
                    prepare.payload(),
                    prepare.descriptors().len(),
                    BROKER,
                )
                .unwrap();
                assert_eq!(request.request().user(), "alice");
                request.request_id()
            };
            drop(prepare);
            let request = PrepareRequestFrame::new(id, BROKER, self::request());
            run(guest, request);
        });
        Self {
            directory,
            thread: Some(thread),
        }
    }

    pub(super) async fn connection(&self) -> BrokerConnection {
        BrokerConnection::connect(
            target::current_target().await,
            self.directory.path().join("broker.sock").to_str().unwrap(),
            ControlLimits::CURRENT,
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap()
    }

    pub(super) async fn prepare(&self) -> BrokerSession {
        BrokerSession::prepare(
            self.connection().await,
            request(),
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap()
    }

    pub(super) async fn finish(mut self) {
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    }
}

pub(super) fn ready(
    guest: &GuestControlStream,
    request: &PrepareRequestFrame<'static>,
    lease: u32,
) -> worker::TestWorker {
    let mut worker = worker::TestWorker::spawn(request.request().clone(), PREPARATION);
    publish_ready(guest, request, lease, &mut worker);
    worker
}

pub(super) fn publish_ready(
    guest: &GuestControlStream,
    request: &PrepareRequestFrame<'_>,
    lease: u32,
    worker: &mut worker::TestWorker,
) {
    let ready = request
        .ready_frame(PrepareReady::new(PREPARATION, BROKER, lease).unwrap())
        .unwrap();
    let mut bytes = [0; 4096];
    let length = ready.encode_into(&mut bytes).unwrap();
    let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    send_guest_frame(
        guest,
        header,
        &bytes[CONTROL_FRAME_HEADER_BYTES..length],
        &[
            worker.identity.take().unwrap().as_raw_fd(),
            worker.pidfd.as_raw_fd(),
        ],
    )
    .unwrap();
}

pub(super) fn command(
    guest: &GuestControlStream,
    request: &PrepareRequestFrame<'_>,
) -> PreparationControlFrame {
    let mut bytes = [0; 4096];
    let frame = receive_guest_frame(guest, &mut bytes).unwrap();
    PreparationControlFrame::decode_for(
        frame.header().request_id(),
        request,
        PREPARATION,
        frame.header(),
        frame.payload(),
        frame.descriptors().len(),
    )
    .unwrap()
}

pub(super) fn reply(guest: &GuestControlStream, command: PreparationControlFrame, lease: u32) {
    let status = match command.kind() {
        PreparationControlKind::KeepPrepared | PreparationControlKind::Recheck => {
            ControlReplyStatus::Prepared {
                remaining_lease_ms: lease,
            }
        }
        _ => ControlReplyStatus::CleanupRequested,
    };
    let mut bytes = [0; 4096];
    let (header, length) = ControlReplyFrame::new(command, status)
        .unwrap()
        .encode_parts(&mut bytes)
        .unwrap();
    send_guest_frame(guest, header, &bytes[..length], &[]).unwrap();
}

pub(super) fn final_exit(
    guest: &GuestControlStream,
    request: &PrepareRequestFrame<'_>,
    reason: ExitReason,
) {
    let report = ExitReport::new(
        ExecutionStatus::NotStarted,
        Some(ExitStatus::Signaled(15)),
        CleanupStatus::Complete,
        reason,
    )
    .unwrap();
    let empty = OutputStream::new(&[], false, true).unwrap();
    exit_report(guest, request, report, empty);
}

pub(super) fn exit_report(
    guest: &GuestControlStream,
    request: &PrepareRequestFrame<'_>,
    report: ExitReport,
    stdout: OutputStream<'_>,
) {
    let empty = OutputStream::new(&[], false, true).unwrap();
    let exit = ExitFrame::new(
        request,
        PREPARATION,
        report,
        Some(CapturedOutput::new(stdout, empty).unwrap()),
    )
    .unwrap();
    let mut bytes = [0; 4096];
    let (header, length) = exit.encode_parts(&mut bytes).unwrap();
    send_guest_frame(guest, header, &bytes[..length], &[]).unwrap();
}

pub(super) fn send_guest_bytes(guest: &GuestControlStream, mut bytes: &[u8]) {
    drive(guest.as_raw_fd(), libc::POLLOUT, || {
        if bytes.is_empty() {
            return Ok(Poll::Ready(()));
        }
        // SAFETY: guest owns the socket and bytes stays readable during send.
        let written = unsafe {
            libc::send(
                guest.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if written > 0 {
            bytes = &bytes[written as usize..];
            return Ok(if bytes.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            });
        }
        if written == 0 {
            return Err(lasper_guest_runtime::transport::TransportError::WriteZero);
        }
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            Ok(Poll::Pending)
        } else {
            Err(lasper_guest_runtime::transport::TransportError::Syscall(
                error.raw_os_error().unwrap(),
            ))
        }
    })
    .unwrap();
}

pub(super) fn send_guest_fragment(guest: &GuestControlStream, bytes: &[u8]) {
    send_guest_bytes(guest, bytes);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    // Unix socket buffers are released when the peer reads them. Wait for
    // that so the host starts its frame timer before the test advances time.
    loop {
        let mut queued = 0i32;
        // SAFETY: guest owns the socket and TIOCOUTQ writes one live int.
        let result = unsafe { libc::ioctl(guest.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        if queued == 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "host did not consume the frame fragment"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
