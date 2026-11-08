use lasper_guest_protocol::{
    BrokerId, CommandArgv, ControlFrameHeader, ExecutionKind, GuestProcess, MessageKind,
    PayloadCodecError, PreparationControl, PreparationControlFrame, PreparationControlKind,
    PreparationId, PrepareFrameError, PrepareRequest, PrepareRequestFrame, ProjectionRequests,
    MAX_COMMAND_TOTAL_BYTES,
};
use lasper_guest_runtime::session::{
    GuardianContext, PrepareBuffer, PrepareStorage, PrepareStorageError,
};
use lasper_guest_runtime::transport::TransportError;
use std::io::Write;
use std::net::Shutdown;
use std::task::Poll;
use std::time::{Duration, Instant};

use super::{command, control_fixture, guardian_fixture::GuardianProcess};

const BROKER: BrokerId = BrokerId::new([1; 16]);
const PREPARATION: PreparationId = PreparationId::new([2; 16]);

unsafe extern "C" {
    fn mincore(address: *const u8, length: usize, residency: *mut u8) -> i32;
}

fn mapped(address: *const u8) -> bool {
    let mut resident = 0;
    let result = unsafe { mincore(address, 1, &mut resident) };
    if result == -1 {
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(12));
    }
    result == 0
}

fn advance<T>(
    mut operation: impl FnMut() -> Result<Poll<T>, TransportError>,
) -> Result<T, TransportError> {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match operation()? {
            Poll::Ready(result) => return Ok(result),
            Poll::Pending => {
                assert!(Instant::now() < deadline, "Prepare test made no progress");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

pub(super) fn child(context: &mut GuardianContext, name: &str) {
    let limits = context.negotiated_limits();
    let mut reader = context.connection().begin_receive().unwrap();
    let header = advance(|| reader.advance()).unwrap();
    let buffer = PrepareBuffer::new(header, BROKER, limits).unwrap();
    assert_eq!(buffer.as_ref().len(), header.payload_bytes() as usize);
    let address = buffer.as_ref().as_ptr();
    assert!(mapped(address));
    let mut receiver = reader.into_payload(buffer).unwrap();
    if name == "prepare-cancel" {
        assert_eq!(receiver.advance(), Ok(Poll::Pending));
        drop(receiver);
        assert!(!context.connection().can_continue());
        assert!(!mapped(address));
        return;
    }
    if name == "prepare-eof" {
        assert_eq!(
            advance(|| receiver.advance()),
            Err(TransportError::UnexpectedEof)
        );
        drop(receiver);
        assert!(!context.connection().can_continue());
        assert!(!mapped(address));
        return;
    }
    advance(|| receiver.advance()).unwrap();
    let storage = PrepareStorage::from_received(receiver.finish().unwrap()).unwrap();
    if name == "prepare-invalid" {
        assert!(matches!(
            storage.request(),
            Err(PrepareStorageError::Frame(PrepareFrameError::Payload(
                PayloadCodecError::InvalidRequest
            )))
        ));
        drop(storage);
        assert!(!mapped(address));
        return;
    }
    {
        let request = storage.request().unwrap();
        let GuestProcess::Command(argv) = request.request().process() else {
            panic!("expected argv");
        };
        let mut original = vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
        let length = argv.write_nul_terminated_into(&mut original).unwrap();
        original.truncate(length);
        let mut control = [0; 512];
        for request_id in 20..28 {
            control.fill(0xa5);
            let frame = control_fixture::receive(context.connection(), &mut control).unwrap();
            let received = PreparationControlFrame::decode_for(
                request_id,
                &request,
                PREPARATION,
                frame.header(),
                frame.payload(),
                frame.descriptors().len(),
            )
            .unwrap();
            assert_eq!(received.request_id(), request_id);
            assert_eq!(received.kind(), PreparationControlKind::KeepPrepared);
            let mut current = vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
            let length = argv.write_nul_terminated_into(&mut current).unwrap();
            assert_eq!(&current[..length], original);
        }
        assert!(mapped(address));
    }
    drop(storage);
    assert!(!mapped(address));
}

fn stable_request() {
    let mut guardian = GuardianProcess::spawn(command("prepare-stable")).unwrap();
    guardian.release();
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    guardian.hello();
    let mut arguments = vec![
        "/bin/tool".to_owned(),
        "quotes\" backslash\\ tab\t".to_owned(),
    ];
    arguments.extend((0..15).map(|_| "界".repeat(1300)));
    let argv: Vec<_> = arguments.iter().map(String::as_str).collect();
    let request = PrepareRequestFrame::new(
        2,
        BROKER,
        PrepareRequest::new(
            "alice",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&argv).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap(),
    );
    let mut bytes = vec![0; 128 * 1024];
    let length = request.encode_into(&mut bytes).unwrap();
    guardian.control.write_all(&bytes[..length]).unwrap();
    for id in 20..28 {
        let message = PreparationControlFrame::new(
            id,
            PreparationControlKind::KeepPrepared,
            PreparationControl::new(BROKER, PREPARATION),
        );
        let mut payload = [0; 256];
        let (header, length) = message.encode_parts(&mut payload).unwrap();
        guardian.control.write_all(&header.encode()).unwrap();
        guardian.control.write_all(&payload[..length]).unwrap();
    }
    assert!(guardian.wait().success());
}

fn interrupted_prepare(name: &str) {
    let mut guardian = GuardianProcess::spawn(command(name)).unwrap();
    guardian.release();
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    guardian.hello();
    let header = ControlFrameHeader::new(MessageKind::PrepareRequest, 2, 1024, 0).unwrap();
    guardian.control.write_all(&header.encode()).unwrap();
    guardian.control.write_all(b"{\"user\":").unwrap();
    if name == "prepare-eof" {
        guardian.control.shutdown(Shutdown::Write).unwrap();
    }
    assert!(guardian.wait().success());
}

fn malformed_request() {
    let mut guardian = GuardianProcess::spawn(command("prepare-invalid")).unwrap();
    guardian.release();
    guardian
        .control
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    guardian.hello();
    let payload =
        br#"{"user":"bad/name","execution":"bounded","process":{"command":["/bin/true"]}}"#;
    let header =
        ControlFrameHeader::new(MessageKind::PrepareRequest, 2, payload.len() as u32, 0).unwrap();
    guardian.control.write_all(&header.encode()).unwrap();
    guardian.control.write_all(payload).unwrap();
    assert!(guardian.wait().success());
}

pub(super) const SCENARIOS: &[(&str, fn())] = &[
    (
        "Prepare bytes survive repeated control buffer reuse and are unmapped afterward",
        stable_request,
    ),
    ("cancelled partial Prepare releases its mapping", || {
        interrupted_prepare("prepare-cancel")
    }),
    ("EOF during Prepare releases its mapping", || {
        interrupted_prepare("prepare-eof")
    }),
    (
        "frozen storage still requires semantic request validation",
        malformed_request,
    ),
];
