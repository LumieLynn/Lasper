use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ExecutionKind, GuestProcess, PreparationId,
    PrepareRequest, PrepareRequestFrame, ProjectionRequests, TerminalEnvironment, TerminalSize,
};

pub fn request(execution: ExecutionKind) -> PrepareRequestFrame<'static> {
    let interactive = execution == ExecutionKind::Interactive;
    PrepareRequestFrame::new(
        7,
        BrokerId::new([255; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/true"]).unwrap()),
            interactive.then(|| TerminalEnvironment::new("xterm-256color", None, None).unwrap()),
            interactive.then(|| TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::new(None, None),
        )
        .unwrap(),
    )
}

pub fn preparation() -> PreparationId {
    PreparationId::new([255; 16])
}

pub fn resized(header: ControlFrameHeader, length: usize) -> ControlFrameHeader {
    ControlFrameHeader::new(header.kind(), header.request_id(), length as u32, 0).unwrap()
}
