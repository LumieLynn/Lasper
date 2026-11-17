use super::{bootstrap::GuardianProcess, wire};
use lasper_guest_protocol::*;
use std::os::fd::{AsRawFd, OwnedFd};

pub fn protocol() -> NegotiatedProtocol {
    negotiate_hello(
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloOffer::local(ControlLimits::CURRENT),
    )
    .unwrap()
}

pub struct Client<'a> {
    pub guardian: GuardianProcess,
    pub request: PrepareRequestFrame<'a>,
    pub ready: PrepareReadyFrame,
    pub descriptors: Vec<OwnedFd>,
    pub next_id: u64,
}

impl<'a> Client<'a> {
    pub fn prepared(
        name: &str,
        privileged: bool,
        execution: ExecutionKind,
        argv: &[&'static str],
    ) -> Self {
        let mut guardian = GuardianProcess::spawn(super::command(name, privileged)).unwrap();
        guardian.release();
        guardian.hello();
        Self::prepare(guardian, execution, argv)
    }

    pub fn prepare(
        guardian: GuardianProcess,
        execution: ExecutionKind,
        argv: &[&'static str],
    ) -> Self {
        Self::prepare_request(guardian, request(execution, argv))
    }

    pub fn prepare_request(guardian: GuardianProcess, request: PrepareRequestFrame<'a>) -> Self {
        let mut bytes = vec![0; MAX_CONTROL_FRAME_BYTES as usize];
        let length = super::client::protocol()
            .encode_prepare_frame(&request, &mut bytes)
            .unwrap();
        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
        wire::send(
            guardian.control.as_raw_fd(),
            header,
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            &[],
        )
        .unwrap();
        let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
        let ready = super::client::protocol()
            .decode_ready(
                &request,
                frame.header,
                &frame.payload,
                frame.descriptors.len(),
            )
            .unwrap();
        Self {
            guardian,
            request,
            ready,
            descriptors: frame.descriptors,
            next_id: 11,
        }
    }

    pub fn drain_output(
        &self,
    ) -> (
        std::thread::JoinHandle<Vec<u8>>,
        std::thread::JoinHandle<Vec<u8>>,
    ) {
        fn read(fd: OwnedFd) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                use std::io::Read;
                // This fixture reads on blocking threads; Ready uses nonblocking FDs.
                let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
                assert!(flags >= 0);
                assert_eq!(
                    unsafe {
                        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK)
                    },
                    0
                );
                let mut bytes = Vec::new();
                std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
                bytes
            })
        }
        (
            read(self.descriptors[2].try_clone().unwrap()),
            read(self.descriptors[3].try_clone().unwrap()),
        )
    }

    pub fn start(&mut self) {
        self.start_with_timeout(None);
    }

    pub fn start_with_timeout(&mut self, timeout_ms: Option<u64>) {
        let command = StartFrame::new(
            self.next_id,
            StartRequest::new(
                &self.request,
                self.ready.response().preparation_id(),
                self.request.request().initial_size(),
                timeout_ms,
            )
            .unwrap(),
        );
        self.next_id += 1;
        self.send(|bytes| {
            super::client::protocol()
                .encode_start_frame(&command, bytes)
                .unwrap()
        });
        let response = self.receive();
        super::client::protocol()
            .decode_start_accepted(
                command.request_id(),
                &self.request,
                self.ready.response().preparation_id(),
                response.header,
                &response.payload,
                response.descriptors.len(),
            )
            .unwrap();
    }

    pub fn control(&mut self, kind: PreparationControlKind) -> ControlReplyFrame {
        let command = PreparationControlFrame::new(
            self.next_id,
            kind,
            PreparationControl::new(
                self.request.broker_id(),
                self.ready.response().preparation_id(),
            ),
        );
        self.next_id += 1;
        self.send(|bytes| {
            super::client::protocol()
                .encode_control_request(&command, bytes)
                .unwrap()
        });
        let response = self.receive();
        super::client::protocol()
            .decode_control_reply(
                &command,
                response.header,
                &response.payload,
                response.descriptors.len(),
            )
            .unwrap()
    }

    pub fn send_control(&mut self, kind: PreparationControlKind) {
        let command = PreparationControlFrame::new(
            self.next_id,
            kind,
            PreparationControl::new(
                self.request.broker_id(),
                self.ready.response().preparation_id(),
            ),
        );
        self.next_id += 1;
        self.send(|bytes| {
            super::client::protocol()
                .encode_control_request(&command, bytes)
                .unwrap()
        });
    }

    pub fn send(&self, encode: impl FnOnce(&mut [u8]) -> (ControlFrameHeader, usize)) {
        let mut payload = vec![0; 16 * 1024];
        let (header, length) = encode(&mut payload);
        wire::send(
            self.guardian.control.as_raw_fd(),
            header,
            &payload[..length],
            &[],
        )
        .unwrap();
    }

    pub fn receive(&self) -> wire::Frame {
        wire::receive(self.guardian.control.as_raw_fd()).unwrap()
    }

    pub fn finish(&mut self) -> wire::Frame {
        loop {
            let frame = self.receive();
            let report = super::client::protocol()
                .decode_exit(
                    &self.request,
                    self.ready.response().preparation_id(),
                    frame.header,
                    &frame.payload,
                    frame.descriptors.len(),
                )
                .unwrap();
            if report.report().is_final() {
                return frame;
            }
        }
    }

    pub fn wait(&mut self) {
        assert!(self.guardian.wait().success());
        let path =
            std::env::temp_dir().join(format!("lasper-account-test-{}", self.guardian.child.id()));
        std::fs::remove_dir_all(path).unwrap();
    }
}

pub fn request(execution: ExecutionKind, argv: &[&'static str]) -> PrepareRequestFrame<'static> {
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(argv).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            (execution == ExecutionKind::Terminal).then(|| TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    )
}
