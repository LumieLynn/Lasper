use super::{bootstrap::GuardianProcess, wire};
use lasper_guest_protocol::*;
use std::os::fd::{AsRawFd, OwnedFd};

pub struct Client {
    pub guardian: GuardianProcess,
    pub request: PrepareRequestFrame<'static>,
    pub ready: PrepareReadyFrame,
    pub descriptors: Vec<OwnedFd>,
    pub next_id: u64,
}

impl Client {
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
        let request = request(execution, argv);
        let mut bytes = vec![0; MAX_CONTROL_FRAME_BYTES as usize];
        let length = request.encode_into(&mut bytes).unwrap();
        let header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
        wire::send(
            guardian.control.as_raw_fd(),
            header,
            &bytes[CONTROL_FRAME_HEADER_BYTES..length],
            &[],
        )
        .unwrap();
        let frame = wire::receive(guardian.control.as_raw_fd()).unwrap();
        let ready = request
            .decode_ready_parts(frame.header, &frame.payload, frame.descriptors.len())
            .unwrap();
        Self {
            guardian,
            request,
            ready,
            descriptors: frame.descriptors,
            next_id: 11,
        }
    }

    pub fn start(&mut self) {
        let command = StartFrame::new(
            self.next_id,
            StartRequest::new(
                &self.request,
                self.ready.response().preparation_id(),
                None,
                None,
                self.request.request().initial_size(),
            )
            .unwrap(),
        );
        self.next_id += 1;
        self.send(|bytes| command.encode_parts(bytes).unwrap());
        let response = self.receive();
        StartAcceptedFrame::decode_for(
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
        self.send(|bytes| command.encode_parts(bytes).unwrap());
        let response = self.receive();
        ControlReplyFrame::decode_for(
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
        self.send(|bytes| command.encode_parts(bytes).unwrap());
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
            let report = ExitFrame::decode_for(
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
            (execution == ExecutionKind::Interactive)
                .then(|| TerminalEnvironment::new("xterm", None, None).unwrap()),
            (execution == ExecutionKind::Interactive).then(|| TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}
