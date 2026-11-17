use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ExecutionKind, GuestProcess, PreparationId,
    PrepareRequest, PrepareRequestFrame, TerminalSize,
};

pub fn protocol() -> lasper_guest_protocol::NegotiatedProtocol {
    lasper_guest_protocol::negotiate_hello(
        lasper_guest_protocol::HelloRequest::new(
            lasper_guest_protocol::FeatureSet::EMPTY,
            lasper_guest_protocol::ControlLimits::CURRENT,
        )
        .unwrap(),
        lasper_guest_protocol::HelloOffer::local(lasper_guest_protocol::ControlLimits::CURRENT),
    )
    .unwrap()
}

pub fn request(execution: ExecutionKind) -> PrepareRequestFrame<'static> {
    let interactive = execution == ExecutionKind::Terminal;
    PrepareRequestFrame::new(
        7,
        BrokerId::new([255; 16]),
        PrepareRequest::new(
            "alice",
            execution,
            GuestProcess::Command(CommandArgv::new(&["/usr/bin/true"]).unwrap()),
            lasper_guest_protocol::ProcessEnvironment::default(),
            interactive.then(|| TerminalSize::new(80, 24).unwrap()),
        )
        .unwrap(),
    )
}

pub fn preparation() -> PreparationId {
    PreparationId::new([255; 16])
}

pub fn resized(header: ControlFrameHeader, length: usize) -> ControlFrameHeader {
    ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        header.kind(),
        header.request_id(),
        length as u32,
        0,
    )
    .unwrap()
}
